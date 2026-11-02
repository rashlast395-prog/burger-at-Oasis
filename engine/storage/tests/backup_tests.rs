use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};

use bo_storage::{manifest_path_for, wal_path_for, BTree, Db, StorageError, PAGE_SIZE};
use tempfile::{tempdir, TempDir};

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn key(n: u64) -> Vec<u8> {
    format!("key-{n:06}").into_bytes()
}

fn value(n: u64, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u64).wrapping_mul(7).wrapping_add(n) as u8).collect()
}

fn populated() -> (TempDir, PathBuf, Db, BTree, Model) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("src.boasis");
    let mut db = Db::create(&path).unwrap();
    let mut t = db.begin();
    let tree = BTree::create(&mut t).unwrap();
    let mut model = Model::new();
    for n in 0..300u64 {
        let len = if n % 10 == 0 { 20_000 } else { 100 }; // some overflow values
        let v = value(n, len);
        tree.insert(&mut t, &key(n), &v).unwrap();
        model.insert(key(n), v);
    }
    t.commit().unwrap();
    (dir, path, db, tree, model)
}

fn assert_tree_equals(db: &mut Db, tree: &BTree, model: &Model) {
    assert_eq!(tree.check(db).unwrap(), model.len());
    let all = tree.range(db, Bound::Unbounded, Bound::Unbounded, None).unwrap();
    let want: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert!(all == want, "tree contents differ from the model");
}

fn flip(path: &Path, offset: u64) {
    let mut f = OpenOptions::new().read(true).write(true).open(path).unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&[0x5A]).unwrap();
}

#[test]
fn backup_then_restore_reproduces_the_database_exactly() {
    let (dir, _p, mut db, tree, model) = populated();
    let backup = dir.path().join("b1.bak");
    let info = db.backup_to(&backup).unwrap();
    assert!(info.page_count > 10);
    assert_eq!(Db::verify_backup(&backup).unwrap(), info);

    let target = dir.path().join("restored.boasis");
    Db::restore(&backup, &target).unwrap();
    let mut restored = Db::open(&target).unwrap();
    assert_tree_equals(&mut restored, &tree, &model);
    assert!(restored.verify().unwrap().is_empty());
}

#[test]
fn a_backup_includes_committed_data_that_was_never_checkpointed() {
    let (dir, path, db, tree, mut model) = populated();
    drop(db); // crash: everything is only in the log
    let mut db = Db::open(&path).unwrap();
    let mut t = db.begin();
    tree.insert(&mut t, b"late", b"committed just before the backup").unwrap();
    t.commit().unwrap();
    model.insert(b"late".to_vec(), b"committed just before the backup".to_vec());
    let backup = dir.path().join("b.bak");
    db.backup_to(&backup).unwrap();
    let target = dir.path().join("r.boasis");
    Db::restore(&backup, &target).unwrap();
    assert_tree_equals(&mut Db::open(&target).unwrap(), &tree, &model);
}

#[test]
fn a_restored_database_is_independent_of_the_original() {
    let (dir, _p, mut db, tree, model) = populated();
    let backup = dir.path().join("b.bak");
    db.backup_to(&backup).unwrap();
    let target = dir.path().join("r.boasis");
    Db::restore(&backup, &target).unwrap();
    let mut restored = Db::open(&target).unwrap();

    let mut t = restored.begin();
    tree.delete(&mut t, &key(5)).unwrap();
    t.commit().unwrap();
    let mut t = db.begin();
    tree.insert(&mut t, b"only-original", b"x").unwrap();
    t.commit().unwrap();

    assert_eq!(tree.get(&mut db, &key(5)).unwrap(), model.get(&key(5)).cloned());
    assert_eq!(tree.get(&mut restored, b"only-original").unwrap(), None);
    Db::verify_backup(&backup).unwrap(); // the backup itself is untouched
}

#[test]
fn verification_catches_every_kind_of_damage() {
    let (dir, _p, mut db, _t, _m) = populated();
    let make = |name: &str, db: &mut Db| -> PathBuf {
        let b = dir.path().join(name);
        db.backup_to(&b).unwrap();
        b
    };

    // flipped byte inside the image
    let b = make("flip.bak", &mut db);
    flip(&b, 3 * PAGE_SIZE as u64 + 77);
    assert!(matches!(Db::verify_backup(&b), Err(StorageError::BackupInvalid(m)) if m.contains("SHA-256")));

    // truncated image
    let b = make("trunc.bak", &mut db);
    OpenOptions::new().write(true).open(&b).unwrap().set_len(5 * PAGE_SIZE as u64).unwrap();
    assert!(matches!(Db::verify_backup(&b), Err(StorageError::BackupInvalid(_))));

    // extra bytes appended
    let b = make("tail.bak", &mut db);
    OpenOptions::new().append(true).open(&b).unwrap().write_all(&[0u8; 100]).unwrap();
    assert!(matches!(Db::verify_backup(&b), Err(StorageError::BackupInvalid(_))));

    // missing manifest
    let b = make("nomanifest.bak", &mut db);
    std::fs::remove_file(manifest_path_for(&b)).unwrap();
    assert!(matches!(Db::verify_backup(&b), Err(StorageError::BackupInvalid(_))));

    // altered manifest (e.g. someone edits the page count or hash)
    let b = make("tamper.bak", &mut db);
    let m = manifest_path_for(&b);
    let text = std::fs::read_to_string(&m).unwrap().replace("page_count=", "page_count=1");
    std::fs::write(&m, text).unwrap();
    assert!(matches!(Db::verify_backup(&b), Err(StorageError::BackupInvalid(m)) if m.contains("manifest")));

    // manifest of a different backup
    let a = make("a.bak", &mut db);
    let mut t = db.begin();
    BTree::create(&mut t).unwrap();
    t.commit().unwrap();
    let c = make("c.bak", &mut db);
    std::fs::copy(manifest_path_for(&c), manifest_path_for(&a)).unwrap();
    assert!(matches!(Db::verify_backup(&a), Err(StorageError::BackupInvalid(_))));

    // an undamaged backup still verifies
    let ok = make("ok.bak", &mut db);
    Db::verify_backup(&ok).unwrap();
}

#[test]
fn a_corrupt_source_never_produces_a_backup() {
    let (dir, path, db, _t, _m) = populated();
    db.close().unwrap();
    flip(&path, 4 * PAGE_SIZE as u64 + 200);
    let mut db = Db::open(&path).unwrap();
    let backup = dir.path().join("bad.bak");
    assert!(matches!(db.backup_to(&backup), Err(StorageError::Corrupt { .. })));
    assert!(!backup.exists());
    assert!(!manifest_path_for(&backup).exists());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("bad.bak"))
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
}

#[test]
fn nothing_is_ever_overwritten() {
    let (dir, _p, mut db, _t, _m) = populated();
    let backup = dir.path().join("b.bak");
    db.backup_to(&backup).unwrap();
    let before = std::fs::read(&backup).unwrap();
    assert!(matches!(db.backup_to(&backup), Err(StorageError::Io(_))));
    assert_eq!(std::fs::read(&backup).unwrap(), before);

    let target = dir.path().join("t.boasis");
    Db::restore(&backup, &target).unwrap();
    let before = std::fs::read(&target).unwrap();
    assert!(matches!(Db::restore(&backup, &target), Err(StorageError::Io(_))));
    assert_eq!(std::fs::read(&target).unwrap(), before);
    // an existing log alone also blocks a restore
    let other = dir.path().join("o.boasis");
    std::fs::write(wal_path_for(&other), b"x").unwrap();
    assert!(matches!(Db::restore(&backup, &other), Err(StorageError::Io(_))));
    assert!(!other.exists());
}

#[test]
fn restoring_a_damaged_backup_is_refused_and_leaves_nothing() {
    let (dir, _p, mut db, _t, _m) = populated();
    let backup = dir.path().join("b.bak");
    db.backup_to(&backup).unwrap();
    flip(&backup, 2 * PAGE_SIZE as u64 + 9);
    let target = dir.path().join("t.boasis");
    assert!(matches!(Db::restore(&backup, &target), Err(StorageError::BackupInvalid(_))));
    assert!(!target.exists());
    assert!(!wal_path_for(&target).exists());
}

// Each backup must be one consistent point in time: restoring it later gives exactly the
// state the database had when the backup was taken, whatever happened afterwards.
#[test]
fn randomized_backups_restore_to_their_own_point_in_time() {
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }
    for seed in 1..=3u64 {
        let dir = tempdir().unwrap();
        let path = dir.path().join("live.boasis");
        let mut db = Db::create(&path).unwrap();
        let mut t = db.begin();
        let tree = BTree::create(&mut t).unwrap();
        t.commit().unwrap();
        db.set_checkpoint_pages(1 + (seed as usize) * 7);
        let mut rng = Rng(seed * 999);
        let mut model = Model::new();
        let mut snapshots: Vec<(PathBuf, Model)> = Vec::new();

        for round in 0..40 {
            let mut t = db.begin();
            for _ in 0..1 + rng.below(20) {
                let k = key(rng.below(200));
                if rng.below(3) == 0 {
                    tree.delete(&mut t, &k).unwrap();
                    model.remove(&k);
                } else {
                    let len = [10usize, 200, 1500, 5000, 40_000][rng.below(5) as usize];
                    let v = value(rng.below(1000), len);
                    tree.insert(&mut t, &k, &v).unwrap();
                    model.insert(k, v);
                }
            }
            t.commit().unwrap();
            if rng.below(8) == 0 {
                drop(db); // crash
                db = Db::open(&path).unwrap();
                db.set_checkpoint_pages(1 + (seed as usize) * 7);
            }
            if round % 7 == 3 {
                let b = dir.path().join(format!("snap{round}.bak"));
                db.backup_to(&b).unwrap();
                snapshots.push((b, model.clone()));
            }
        }
        for (i, (b, snap)) in snapshots.iter().enumerate() {
            Db::verify_backup(b).unwrap();
            let target = dir.path().join(format!("restored{i}.boasis"));
            Db::restore(b, &target).unwrap();
            let mut r = Db::open(&target).unwrap();
            assert_tree_equals(&mut r, &tree, snap);
            assert!(r.verify().unwrap().is_empty());
        }
        assert!(!snapshots.is_empty());
    }
}
