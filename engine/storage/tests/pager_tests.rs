use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};

use bo_storage::{Page, PageKind, Pager, StorageError, PAGE_SIZE};
use tempfile::tempdir;

fn data_page(fill: u8) -> Page {
    let mut p = Page::new(PageKind::Data);
    p.payload_mut().iter_mut().for_each(|b| *b = fill);
    p
}

/// Flip one byte of the file on disk, simulating bit rot or a torn write.
fn corrupt_byte(path: &std::path::Path, offset: u64) {
    let mut f = OpenOptions::new().read(true).write(true).open(path).unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&[0xFF]).unwrap();
    f.sync_all().unwrap();
}

#[test]
fn created_database_reopens_with_header_intact() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.boasis");
    {
        let p = Pager::create(&path).unwrap();
        assert_eq!(p.page_count(), 1);
    }
    let p = Pager::open(&path).unwrap();
    assert_eq!(p.page_count(), 1);
}

#[test]
fn creating_over_an_existing_file_is_refused() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.boasis");
    Pager::create(&path).unwrap();
    assert!(matches!(Pager::create(&path), Err(StorageError::Io(_))));
}

#[test]
fn written_pages_survive_close_and_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.boasis");
    let (a, b);
    {
        let mut p = Pager::create(&path).unwrap();
        a = p.allocate().unwrap();
        b = p.allocate().unwrap();
        p.write(a, &data_page(0xAA)).unwrap();
        p.write(b, &data_page(0xBB)).unwrap();
        p.sync().unwrap();
    }
    let mut p = Pager::open(&path).unwrap();
    assert_eq!(p.page_count(), 3);
    assert_eq!(p.read(a).unwrap(), data_page(0xAA));
    assert_eq!(p.read(b).unwrap(), data_page(0xBB));
    assert!(p.verify().unwrap().is_empty());
}

#[test]
fn freed_pages_are_reused_most_recent_first() {
    let dir = tempdir().unwrap();
    let mut p = Pager::create(dir.path().join("t.boasis")).unwrap();
    let a = p.allocate().unwrap();
    let b = p.allocate().unwrap();
    let c = p.allocate().unwrap();
    p.free(a).unwrap();
    p.free(c).unwrap();
    assert_eq!(p.allocate().unwrap(), c);
    assert_eq!(p.allocate().unwrap(), a);
    // List is empty again, so the file grows.
    let d = p.allocate().unwrap();
    assert!(d > c && d > b);
    assert!(p.verify().unwrap().is_empty());
}

#[test]
fn free_list_survives_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.boasis");
    let a;
    {
        let mut p = Pager::create(&path).unwrap();
        a = p.allocate().unwrap();
        p.allocate().unwrap();
        p.free(a).unwrap();
        p.sync().unwrap();
    }
    let mut p = Pager::open(&path).unwrap();
    assert_eq!(p.allocate().unwrap(), a);
}

#[test]
fn double_free_and_writing_a_freed_page_are_rejected() {
    let dir = tempdir().unwrap();
    let mut p = Pager::create(dir.path().join("t.boasis")).unwrap();
    let a = p.allocate().unwrap();
    p.free(a).unwrap();
    assert!(matches!(p.free(a), Err(StorageError::InvalidPage(_))));
    assert!(matches!(p.write(a, &data_page(1)), Err(StorageError::InvalidPage(_))));
}

#[test]
fn reserved_and_out_of_range_pages_are_rejected() {
    let dir = tempdir().unwrap();
    let mut p = Pager::create(dir.path().join("t.boasis")).unwrap();
    let a = p.allocate().unwrap();
    assert!(matches!(p.read(0), Err(StorageError::InvalidPage(0))));
    assert!(matches!(p.write(0, &data_page(1)), Err(StorageError::InvalidPage(0))));
    assert!(matches!(p.read(99), Err(StorageError::OutOfRange(99))));
    assert!(matches!(p.write(99, &data_page(1)), Err(StorageError::OutOfRange(99))));
    // Callers cannot forge header or free pages.
    assert!(matches!(p.write(a, &Page::new(PageKind::Header)), Err(StorageError::InvalidPage(_))));
    assert!(matches!(p.write(a, &Page::new(PageKind::Free)), Err(StorageError::InvalidPage(_))));
}

#[test]
fn a_flipped_byte_in_a_data_page_is_detected_not_returned() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.boasis");
    let a;
    {
        let mut p = Pager::create(&path).unwrap();
        a = p.allocate().unwrap();
        p.write(a, &data_page(0x11)).unwrap();
        p.sync().unwrap();
    }
    corrupt_byte(&path, a * PAGE_SIZE as u64 + 100);
    let mut p = Pager::open(&path).unwrap(); // header is fine
    assert!(matches!(p.read(a), Err(StorageError::Corrupt { .. })));
    let problems = p.verify().unwrap();
    assert_eq!(problems.len(), 1);
    assert!(matches!(problems[0], StorageError::Corrupt { page, .. } if page == a));
}

#[test]
fn a_zeroed_page_is_reported_as_corrupt() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.boasis");
    let a;
    {
        let mut p = Pager::create(&path).unwrap();
        a = p.allocate().unwrap();
        p.write(a, &data_page(0x22)).unwrap();
        p.sync().unwrap();
    }
    {
        let mut f = OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(a * PAGE_SIZE as u64)).unwrap();
        f.write_all(&[0u8; PAGE_SIZE]).unwrap();
    }
    let mut p = Pager::open(&path).unwrap();
    assert!(matches!(p.read(a), Err(StorageError::Corrupt { .. })));
}

#[test]
fn verify_reports_every_damaged_page() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.boasis");
    let ids: Vec<u64>;
    {
        let mut p = Pager::create(&path).unwrap();
        ids = (0..5).map(|_| p.allocate().unwrap()).collect();
        for (i, id) in ids.iter().enumerate() {
            p.write(*id, &data_page(i as u8 + 1)).unwrap();
        }
        p.sync().unwrap();
    }
    corrupt_byte(&path, ids[1] * PAGE_SIZE as u64 + 50);
    corrupt_byte(&path, ids[3] * PAGE_SIZE as u64 + 50);
    let mut p = Pager::open(&path).unwrap();
    assert_eq!(p.verify().unwrap().len(), 2);
}

#[test]
fn a_damaged_header_prevents_opening() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.boasis");
    Pager::create(&path).unwrap();
    corrupt_byte(&path, 20);
    assert!(matches!(Pager::open(&path), Err(StorageError::Corrupt { page: 0, .. })));
}

#[test]
fn wrong_magic_and_short_files_are_rejected() {
    let dir = tempdir().unwrap();
    let junk = dir.path().join("junk");
    std::fs::write(&junk, b"hello").unwrap();
    assert!(matches!(Pager::open(&junk), Err(StorageError::BadHeader(_))));

    let other = dir.path().join("other");
    std::fs::write(&other, vec![0u8; PAGE_SIZE]).unwrap();
    assert!(matches!(Pager::open(&other), Err(StorageError::Corrupt { .. })));
}

#[test]
fn a_truncated_file_is_detected_on_open() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.boasis");
    {
        let mut p = Pager::create(&path).unwrap();
        for _ in 0..4 {
            p.allocate().unwrap();
        }
        p.sync().unwrap();
    }
    let f = OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(2 * PAGE_SIZE as u64).unwrap();
    drop(f);
    assert!(matches!(Pager::open(&path), Err(StorageError::BadHeader(_))));
}

#[test]
fn an_extra_tail_from_an_interrupted_extension_is_tolerated() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.boasis");
    let a;
    {
        let mut p = Pager::create(&path).unwrap();
        a = p.allocate().unwrap();
        p.write(a, &data_page(7)).unwrap();
        p.sync().unwrap();
    }
    // Simulate: a page was written past the end, but the header was never updated.
    let mut f = OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(SeekFrom::End(0)).unwrap();
    f.write_all(&[0xABu8; PAGE_SIZE]).unwrap();
    drop(f);
    let mut p = Pager::open(&path).unwrap();
    assert_eq!(p.page_count(), 2);
    assert_eq!(p.read(a).unwrap(), data_page(7));
    assert!(p.verify().unwrap().is_empty());
}
