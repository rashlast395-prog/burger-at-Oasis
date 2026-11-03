use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use bo_storage::{wal_path_for, Db, Page, PageId, PageKind, StorageError, PAGE_SIZE};
use tempfile::{tempdir, TempDir};

fn data_page(fill: u8) -> Page {
    let mut p = Page::new(PageKind::Data);
    p.payload_mut().iter_mut().for_each(|b| *b = fill);
    p
}

fn fill_of(p: &Page) -> u8 {
    let b = p.payload();
    assert!(b.iter().all(|x| *x == b[0]), "page payload is not uniform");
    b[0]
}

fn new_db() -> (TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.boasis");
    Db::create(&path).unwrap().close().unwrap();
    (dir, path)
}

fn wal_len(path: &Path) -> u64 {
    std::fs::metadata(wal_path_for(path)).unwrap().len()
}

fn truncate_wal(path: &Path, len: u64) {
    OpenOptions::new().write(true).open(wal_path_for(path)).unwrap().set_len(len).unwrap();
}

fn flip_wal_byte(path: &Path, offset: u64) {
    let mut f = OpenOptions::new().read(true).write(true).open(wal_path_for(path)).unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&[0xFF]).unwrap();
}

/// Commit one transaction that allocates a page holding `fill`.
fn commit_page(db: &mut Db, fill: u8) -> PageId {
    let mut t = db.begin();
    let id = t.allocate().unwrap();
    t.write(id, &data_page(fill)).unwrap();
    t.commit().unwrap();
    id
}

#[test]
fn committed_data_survives_a_crash_before_checkpoint() {
    let (_d, path) = new_db();
    let id;
    {
        let mut db = Db::open(&path).unwrap();
        id = commit_page(&mut db, 0xAA);
        // dropped without close(): the main file has never seen this page
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(fill_of(&db.read(id).unwrap()), 0xAA);
    assert!(db.verify().unwrap().is_empty());
}

#[test]
fn uncommitted_and_rolled_back_work_is_invisible() {
    let (_d, path) = new_db();
    let mut db = Db::open(&path).unwrap();
    let kept = commit_page(&mut db, 1);
    {
        let mut t = db.begin();
        let id = t.allocate().unwrap();
        t.write(id, &data_page(9)).unwrap();
        t.write(kept, &data_page(9)).unwrap();
        // dropped: rollback
    }
    assert_eq!(db.page_count(), 2);
    assert_eq!(fill_of(&db.read(kept).unwrap()), 1);
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.page_count(), 2);
    assert_eq!(fill_of(&db.read(kept).unwrap()), 1);
}

#[test]
fn a_transaction_sees_its_own_writes_but_others_do_not_until_commit() {
    let (_d, path) = new_db();
    let mut db = Db::open(&path).unwrap();
    let id = commit_page(&mut db, 1);
    let mut t = db.begin();
    t.write(id, &data_page(2)).unwrap();
    assert_eq!(fill_of(&t.read(id).unwrap()), 2);
    drop(t);
    assert_eq!(fill_of(&db.read(id).unwrap()), 1);
}

#[test]
fn a_torn_final_commit_is_dropped_and_earlier_commits_survive() {
    let (_d, path) = new_db();
    let (a, b, before_b);
    {
        let mut db = Db::open(&path).unwrap();
        a = commit_page(&mut db, 0x11);
        before_b = db.wal_len();
        b = commit_page(&mut db, 0x22);
    }
    let full = wal_len(&path);
    // Cut the second commit at several points: mid-record, midway, and one byte short.
    for cut in [before_b + 10, before_b + (full - before_b) / 2, full - 1] {
        let copy = path.with_extension(format!("cut{cut}"));
        std::fs::copy(&path, &copy).unwrap();
        std::fs::copy(wal_path_for(&path), wal_path_for(&copy)).unwrap();
        truncate_wal(&copy, cut);
        let db = Db::open(&copy).unwrap();
        assert_eq!(fill_of(&db.read(a).unwrap()), 0x11, "cut at {cut}");
        assert!(matches!(db.read(b), Err(StorageError::OutOfRange(_))), "cut at {cut}");
        assert!(db.verify().unwrap().is_empty());
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(fill_of(&db.read(b).unwrap()), 0x22);
}

#[test]
fn trailing_garbage_in_the_log_is_ignored() {
    let (_d, path) = new_db();
    let id;
    {
        let mut db = Db::open(&path).unwrap();
        id = commit_page(&mut db, 0x33);
    }
    let mut f = OpenOptions::new().append(true).open(wal_path_for(&path)).unwrap();
    f.write_all(&[0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18]).unwrap();
    drop(f);
    let mut db = Db::open(&path).unwrap();
    assert_eq!(fill_of(&db.read(id).unwrap()), 0x33);
    // the log is usable again afterwards
    let id2 = commit_page(&mut db, 0x44);
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(fill_of(&db.read(id2).unwrap()), 0x44);
}

#[test]
fn damage_in_the_middle_of_the_log_refuses_to_open_instead_of_losing_commits() {
    let (_d, path) = new_db();
    {
        let mut db = Db::open(&path).unwrap();
        commit_page(&mut db, 1);
        commit_page(&mut db, 2);
    }
    flip_wal_byte(&path, 16 + 200); // inside the first transaction's page record
    assert!(matches!(Db::open(&path), Err(StorageError::WalCorrupt(_))));
}

#[test]
fn a_damaged_main_file_header_is_repaired_from_the_log() {
    let (_d, path) = new_db();
    let id;
    {
        let mut db = Db::open(&path).unwrap();
        id = commit_page(&mut db, 0x55); // logs page 0 as well (page count changed)
    }
    // Simulate a crash in the middle of a checkpoint that tore the header.
    let mut f = OpenOptions::new().read(true).write(true).open(&path).unwrap();
    f.seek(SeekFrom::Start(20)).unwrap();
    f.write_all(&[0xFF; 8]).unwrap();
    drop(f);
    let db = Db::open(&path).unwrap();
    assert_eq!(fill_of(&db.read(id).unwrap()), 0x55);
    assert!(db.verify().unwrap().is_empty());
}

#[test]
fn replaying_a_log_twice_is_harmless() {
    let (_d, path) = new_db();
    let (a, b);
    {
        let mut db = Db::open(&path).unwrap();
        a = commit_page(&mut db, 7);
        b = commit_page(&mut db, 8);
    }
    let saved = std::fs::read(wal_path_for(&path)).unwrap();
    Db::open(&path).unwrap().close().unwrap(); // replay + checkpoint empties the log
    assert_eq!(wal_len(&path), 16);
    // Crash after copying pages but before the log was emptied: the log comes back.
    std::fs::write(wal_path_for(&path), &saved).unwrap();
    let db = Db::open(&path).unwrap();
    assert_eq!(fill_of(&db.read(a).unwrap()), 7);
    assert_eq!(fill_of(&db.read(b).unwrap()), 8);
    assert_eq!(db.page_count(), 3);
    assert!(db.verify().unwrap().is_empty());
}

#[test]
fn a_missing_log_is_an_error_not_a_silent_reset() {
    let (_d, path) = new_db();
    std::fs::remove_file(wal_path_for(&path)).unwrap();
    assert!(matches!(Db::open(&path), Err(StorageError::Io(_))));
}

#[test]
fn free_list_changes_are_transactional_and_durable() {
    let (_d, path) = new_db();
    let (a, b);
    {
        let mut db = Db::open(&path).unwrap();
        a = commit_page(&mut db, 1);
        b = commit_page(&mut db, 2);
        let mut t = db.begin();
        t.free(a).unwrap();
        assert!(matches!(t.free(a), Err(StorageError::InvalidPage(_))));
        assert!(matches!(t.write(a, &data_page(5)), Err(StorageError::InvalidPage(_))));
        t.commit().unwrap();
        // a rolled-back allocation must not consume the freed page
        let mut t = db.begin();
        assert_eq!(t.allocate().unwrap(), a);
        drop(t);
    }
    let db = Db::open(&path).unwrap();
    let mut t = db.begin();
    assert_eq!(t.allocate().unwrap(), a, "freed page is reused after recovery");
    t.write(a, &data_page(9)).unwrap();
    t.commit().unwrap();
    assert_eq!(fill_of(&db.read(b).unwrap()), 2);
    assert!(db.verify().unwrap().is_empty());
}

#[test]
fn reserved_and_out_of_range_pages_are_rejected_in_transactions() {
    let (_d, path) = new_db();
    let mut db = Db::open(&path).unwrap();
    let a = commit_page(&mut db, 1);
    let mut t = db.begin();
    assert!(matches!(t.read(0), Err(StorageError::InvalidPage(0))));
    assert!(matches!(t.read(99), Err(StorageError::OutOfRange(99))));
    assert!(matches!(t.write(a, &Page::new(PageKind::Header)), Err(StorageError::InvalidPage(_))));
    assert!(matches!(t.write(a, &Page::new(PageKind::Free)), Err(StorageError::InvalidPage(_))));
}

#[test]
fn automatic_checkpoint_empties_the_log_and_keeps_the_data() {
    let (_d, path) = new_db();
    let mut db = Db::open(&path).unwrap();
    db.set_checkpoint_pages(4);
    let ids: Vec<_> = (0..10u8).map(|i| commit_page(&mut db, i + 1)).collect();
    // Ten commits of 2 pages each would be > 20 pages of log without checkpoints.
    assert!(db.wal_len() < 10 * 2 * (PAGE_SIZE as u64), "log should have been truncated");
    drop(db);
    let db = Db::open(&path).unwrap();
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(fill_of(&db.read(*id).unwrap()), i as u8 + 1);
    }
    assert!(db.verify().unwrap().is_empty());
}

// ---- randomized crash testing against a simple model ----

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

fn check_matches(db: &mut Db, model: &HashMap<PageId, u8>) {
    for (id, fill) in model {
        assert_eq!(fill_of(&db.read(*id).unwrap()), *fill, "page {id}");
    }
}

/// Apply a random transaction to the database and, if committed, to the model.
fn random_txn(db: &mut Db, model: &mut HashMap<PageId, u8>, rng: &mut Rng, commit: bool) {
    let mut m = model.clone();
    let mut t = db.begin();
    for _ in 0..1 + rng.below(4) {
        match rng.below(3) {
            0 => {
                let id = t.allocate().unwrap();
                let f = rng.below(250) as u8 + 1;
                t.write(id, &data_page(f)).unwrap();
                m.insert(id, f);
            }
            1 if !m.is_empty() => {
                let ids: Vec<_> = m.keys().copied().collect();
                let id = ids[rng.below(ids.len() as u64) as usize];
                let f = rng.below(250) as u8 + 1;
                t.write(id, &data_page(f)).unwrap();
                m.insert(id, f);
            }
            _ if !m.is_empty() => {
                let ids: Vec<_> = m.keys().copied().collect();
                let id = ids[rng.below(ids.len() as u64) as usize];
                t.free(id).unwrap();
                m.remove(&id);
            }
            _ => {}
        }
    }
    if commit {
        t.commit().unwrap();
        *model = m;
    }
}

#[test]
fn randomized_crashes_and_rollbacks_match_the_model() {
    for seed in 1..=8u64 {
        let (_d, path) = new_db();
        let mut rng = Rng(seed);
        let mut model = HashMap::new();
        let mut db = Db::open(&path).unwrap();
        db.set_checkpoint_pages(1 + rng.below(12) as usize);
        for _ in 0..120 {
            let commit = rng.below(5) != 0;
            random_txn(&mut db, &mut model, &mut rng, commit);
            if rng.below(6) == 0 {
                drop(db); // crash
                db = Db::open(&path).unwrap();
                db.set_checkpoint_pages(1 + rng.below(12) as usize);
                check_matches(&mut db, &model);
            }
        }
        check_matches(&mut db, &model);
        assert!(db.verify().unwrap().is_empty(), "seed {seed}");
    }
}

#[test]
fn randomized_torn_last_commit_rolls_back_exactly_that_transaction() {
    for seed in 1..=8u64 {
        let (_d, path) = new_db();
        let mut rng = Rng(seed * 77);
        let mut model = HashMap::new();
        let mut db = Db::open(&path).unwrap();
        db.set_checkpoint_pages(usize::MAX); // keep every commit in the log
        for _ in 0..40 {
            let snapshot = model.clone();
            let before = db.wal_len();
            random_txn(&mut db, &mut model, &mut rng, true);
            let after = db.wal_len();
            if after == before {
                continue; // empty transaction wrote nothing
            }
            if rng.below(5) == 0 {
                drop(db);
                let cut = before + rng.below(after - before); // strictly inside the last commit
                truncate_wal(&path, cut);
                model = snapshot;
                db = Db::open(&path).unwrap();
                db.set_checkpoint_pages(usize::MAX);
                check_matches(&mut db, &model);
            }
        }
        drop(db);
        let mut db = Db::open(&path).unwrap();
        check_matches(&mut db, &model);
        assert!(db.verify().unwrap().is_empty(), "seed {seed}");
    }
}
