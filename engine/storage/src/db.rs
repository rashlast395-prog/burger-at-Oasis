//! Transactional database over the pager and the write-ahead log.
//!
//! * `Db::begin()` starts a transaction; changes are buffered in memory.
//! * `Txn::commit()` appends the changed page images plus a commit record to the WAL and
//!   fsyncs it **before returning**. Only then is the transaction durable and visible.
//! * Dropping a `Txn` without committing discards it. Nothing reaches disk.
//! * Committed pages live in the WAL (and an in-memory overlay) until a checkpoint copies
//!   them into the main file, fsyncs it, and empties the log.
//! * `Db::open` replays committed transactions from the log *before* it trusts the main
//!   file, so a torn header or half-finished checkpoint is repaired from the log.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use crate::page::{Page, PageId, PageKind, PAGE_SIZE};
use crate::pager::{header_image, write_raw, Pager};
use crate::wal::{Image, Wal};
use crate::{Result, StorageError};

/// Checkpoint automatically once this many distinct pages are waiting in the log.
const DEFAULT_CHECKPOINT_PAGES: usize = 256;

pub struct Db {
    pager: Pager,
    wal: Wal,
    /// Latest committed image of every page not yet copied to the main file.
    overlay: BTreeMap<PageId, Image>,
    page_count: u64,
    free_head: PageId,
    next_txn: u64,
    checkpoint_pages: usize,
}

/// Path of the log file that belongs to a database file.
pub fn wal_path_for(db_path: &Path) -> PathBuf {
    let mut s: OsString = db_path.as_os_str().to_owned();
    s.push(".wal");
    PathBuf::from(s)
}

impl Db {
    /// Create a new database and its log. Fails if either file already exists.
    pub fn create(path: impl AsRef<Path>) -> Result<Db> {
        let path = path.as_ref();
        let pager = Pager::create(path)?;
        let wal = Wal::create(wal_path_for(path))?;
        Ok(Db::assemble(pager, wal))
    }

    /// Open an existing database, recovering from the log first.
    ///
    /// Fails if the log is missing: a missing log may mean lost transactions, so it is never
    /// silently recreated.
    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        let path = path.as_ref();
        let (mut wal, committed) = Wal::open(wal_path_for(path))?;
        if !committed.is_empty() {
            // Replay on a raw handle: the main file's header may itself be what needs repair.
            let mut f = OpenOptions::new().read(true).write(true).open(path)?;
            for c in &committed {
                for (id, img) in &c.pages {
                    write_raw(&mut f, *id, img)?;
                }
            }
            f.sync_all()?;
            wal.reset()?;
        }
        let pager = Pager::open(path)?;
        Ok(Db::assemble(pager, wal))
    }

    fn assemble(pager: Pager, wal: Wal) -> Db {
        let page_count = pager.page_count();
        let free_head = pager.free_head();
        Db { pager, wal, overlay: BTreeMap::new(), page_count, free_head, next_txn: 1, checkpoint_pages: DEFAULT_CHECKPOINT_PAGES }
    }

    /// Change the automatic checkpoint threshold (in pages). Mainly for tests.
    pub fn set_checkpoint_pages(&mut self, pages: usize) {
        self.checkpoint_pages = pages.max(1);
    }

    /// Total pages including the header page, as of the last commit.
    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Current size of the log in bytes (header included).
    pub fn wal_len(&self) -> u64 {
        self.wal.len()
    }

    pub fn begin(&mut self) -> Txn<'_> {
        let id = self.next_txn;
        self.next_txn += 1;
        Txn { page_count: self.page_count, free_head: self.free_head, db: self, dirty: BTreeMap::new(), id }
    }

    /// Read the last committed version of a page.
    pub fn read(&mut self, id: PageId) -> Result<Page> {
        if id == 0 {
            return Err(StorageError::InvalidPage(0));
        }
        if id >= self.page_count {
            return Err(StorageError::OutOfRange(id));
        }
        self.read_committed(id)
    }

    fn read_committed(&mut self, id: PageId) -> Result<Page> {
        match self.overlay.get(&id) {
            Some(img) => Page::decode(id, img),
            None => self.pager.read(id),
        }
    }

    /// Copy committed pages into the main file, fsync it, then empty the log.
    pub fn checkpoint(&mut self) -> Result<()> {
        if self.overlay.is_empty() {
            return Ok(());
        }
        for (id, img) in &self.overlay {
            self.pager.raw_write(*id, img)?;
        }
        self.pager.sync()?;
        self.wal.reset()?;
        self.overlay.clear();
        self.pager.refresh_header()
    }

    /// Checkpoint and check every page's checksum and the free list.
    pub fn verify(&mut self) -> Result<Vec<StorageError>> {
        self.checkpoint()?;
        self.pager.verify()
    }

    /// Clean shutdown: checkpoint so the next open has nothing to replay.
    pub fn close(mut self) -> Result<()> {
        self.checkpoint()
    }
}

/// Anything pages can be read from: a [`Db`] (last committed state) or a [`Txn`] (its own view).
pub trait PageRead {
    fn read_page(&mut self, id: PageId) -> Result<Page>;
}

impl PageRead for Db {
    fn read_page(&mut self, id: PageId) -> Result<Page> {
        self.read(id)
    }
}

impl PageRead for Txn<'_> {
    fn read_page(&mut self, id: PageId) -> Result<Page> {
        self.read(id)
    }
}

pub struct Txn<'a> {
    db: &'a mut Db,
    dirty: BTreeMap<PageId, Image>,
    page_count: u64,
    free_head: PageId,
    id: u64,
}

impl Txn<'_> {
    fn check(&self, id: PageId) -> Result<()> {
        if id == 0 {
            return Err(StorageError::InvalidPage(0));
        }
        if id >= self.page_count {
            return Err(StorageError::OutOfRange(id));
        }
        Ok(())
    }

    fn get(&mut self, id: PageId) -> Result<Page> {
        match self.dirty.get(&id) {
            Some(img) => Page::decode(id, img),
            None => self.db.read_committed(id),
        }
    }

    fn put(&mut self, id: PageId, page: &Page) {
        self.dirty.insert(id, Box::new(page.encode()));
    }

    /// Read a page as this transaction sees it (its own writes included).
    pub fn read(&mut self, id: PageId) -> Result<Page> {
        self.check(id)?;
        self.get(id)
    }

    /// Overwrite a page. Only `Unused`, `Data` and `Index` pages may be written.
    pub fn write(&mut self, id: PageId, page: &Page) -> Result<()> {
        self.check(id)?;
        if matches!(page.kind(), PageKind::Header | PageKind::Free) {
            return Err(StorageError::InvalidPage(id));
        }
        if self.get(id)?.kind() == PageKind::Free {
            return Err(StorageError::InvalidPage(id));
        }
        self.put(id, page);
        Ok(())
    }

    /// Reserve a page, reusing a freed one when available.
    pub fn allocate(&mut self) -> Result<PageId> {
        let id = if self.free_head != 0 {
            let id = self.free_head;
            let page = self.get(id)?;
            if page.kind() != PageKind::Free {
                return Err(StorageError::Corrupt { page: id, reason: "free list points at a non-free page".into() });
            }
            self.free_head = u64::from_le_bytes(page.payload()[0..8].try_into().unwrap());
            id
        } else {
            let id = self.page_count;
            self.page_count += 1;
            id
        };
        self.put(id, &Page::new(PageKind::Unused));
        Ok(id)
    }

    /// Return a page to the free list. Freeing a page twice is rejected.
    pub fn free(&mut self, id: PageId) -> Result<()> {
        self.check(id)?;
        if self.get(id)?.kind() == PageKind::Free {
            return Err(StorageError::InvalidPage(id));
        }
        let mut page = Page::new(PageKind::Free);
        page.payload_mut()[0..8].copy_from_slice(&self.free_head.to_le_bytes());
        self.put(id, &page);
        self.free_head = id;
        Ok(())
    }

    /// Make the transaction durable. When this returns `Ok`, the commit survives a crash.
    pub fn commit(mut self) -> Result<()> {
        let header_changed = self.page_count != self.db.page_count || self.free_head != self.db.free_head;
        if self.dirty.is_empty() && !header_changed {
            return Ok(());
        }
        if header_changed {
            self.dirty.insert(0, Box::new(header_image(self.page_count, self.free_head)));
        }
        let images: Vec<(PageId, &[u8; PAGE_SIZE])> = self.dirty.iter().map(|(id, img)| (*id, &**img)).collect();
        self.db.wal.append_txn(self.id, &images)?;

        // Durable. Publish to readers.
        let db = &mut *self.db;
        db.page_count = self.page_count;
        db.free_head = self.free_head;
        db.overlay.extend(std::mem::take(&mut self.dirty));
        if db.overlay.len() >= db.checkpoint_pages {
            db.checkpoint().map_err(|e| StorageError::PostCommit(Box::new(e)))?;
        }
        Ok(())
    }
}
