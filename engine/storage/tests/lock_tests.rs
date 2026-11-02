use bo_storage::{Db, StorageError};
use tempfile::TempDir;

#[test]
fn second_open_is_refused_until_first_is_dropped() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("a.db");
    let db = Db::create(&p).unwrap();
    assert!(matches!(Db::open(&p), Err(StorageError::Locked)));
    // clones share the one lock; the database stays locked while any clone lives
    let clone = db.clone();
    drop(db);
    assert!(matches!(Db::open(&p), Err(StorageError::Locked)));
    drop(clone);
    let again = Db::open(&p).unwrap();
    assert!(matches!(Db::open(&p), Err(StorageError::Locked)));
    drop(again);
    Db::open(&p).unwrap();
}

#[test]
fn refused_open_does_not_touch_the_files() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("a.db");
    let db = Db::create(&p).unwrap();
    let mut t = db.begin();
    let tree = bo_storage::BTree::create(&mut t).unwrap();
    tree.insert(&mut t, b"k", b"v").unwrap();
    t.commit().unwrap();
    let wal_before = std::fs::read(bo_storage::wal_path_for(&p)).unwrap();
    assert!(Db::open(&p).is_err());
    assert_eq!(std::fs::read(bo_storage::wal_path_for(&p)).unwrap(), wal_before);
}
