//! Transactional database over the pager and the write-ahead log, with snapshot readers.
//!
//! * [`Db`] is a cheap, cloneable, thread-safe handle.
//! * **One writer at a time.** [`Db::begin`] takes the writer lock (waiting if another
//!   transaction is open); [`Db::try_begin`] does not wait.
//! * `Txn::commit()` appends the changed page images plus a commit record to the WAL and
//!   fsyncs it **before returning**. Only then is the transaction durable and visible.
//!   Dropping a `Txn` without committing discards it. Nothing reaches disk.
//! * **Any number of readers.** [`Db::snapshot`] pins the committed state at that moment.
//!   Whatever commits afterwards, a snapshot keeps seeing exactly what it saw first, and
//!   readers never wait for the writer or each other.
//! * Committed pages live in the WAL (and an in-memory overlay) until a checkpoint copies
//!   them into the main file. **A checkpoint is skipped while any snapshot is open**, because
//!   it would overwrite pages an older snapshot may still need. A long-lived snapshot
//!   therefore lets the log grow; drop snapshots promptly.
//! * `Db::open` replays committed transactions from the log *before* it trusts the main
//!   file, so a torn header or half-finished checkpoint is repaired from the log.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::page::{Page, PageId, PageKind, PAGE_SIZE};
use crate::pager::{header_image, read_at, write_raw, Pager};
use crate::wal::Wal;
use crate::{Result, StorageError};

/// Checkpoint automatically once this many distinct pages are waiting in the log.
const DEFAULT_CHECKPOINT_PAGES: usize = 256;

type Image = Arc<[u8; PAGE_SIZE]>;

/// An immutable view of the committed database.
struct State {
    /// Latest committed image of every page not yet copied to the main file.
    overlay: BTreeMap<PageId, Image>,
    page_count: u64,
    free_head: PageId,
}

struct Shared {
    current: Arc<State>,
    readers: usize,
}

struct Writer {
    pager: Pager,
    wal: Wal,
    next_txn: u64,
    checkpoint_pages: usize,
}

struct Inner {
    /// Read-only handle used with positional reads only.
    read_file: File,
    shared: Mutex<Shared>,
    writer: Mutex<Writer>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere cannot leave these structures half-updated in a way that matters:
    // durable state lives in the WAL and per-transaction state is local to its `Txn`.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn read_committed(state: &State, file: &File, id: PageId) -> Result<Page> {
    match state.overlay.get(&id) {
        Some(img) => Page::decode(id, img),
        None => Page::decode(id, &read_at(file, id)?),
    }
}

/// Path of the log file that belongs to a database file.
pub fn wal_path_for(db_path: &Path) -> PathBuf {
    let mut s: OsString = db_path.as_os_str().to_owned();
    s.push(".wal");
    PathBuf::from(s)
}

#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

impl Db {
    /// Create a new database and its log. Fails if either file already exists.
    pub fn create(path: impl AsRef<Path>) -> Result<Db> {
        let path = path.as_ref();
        let pager = Pager::create(path)?;
        let wal = Wal::create(wal_path_for(path))?;
        Db::assemble(path, pager, wal)
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
        Db::assemble(path, pager, wal)
    }

    fn assemble(path: &Path, pager: Pager, wal: Wal) -> Result<Db> {
        let state = State { overlay: BTreeMap::new(), page_count: pager.page_count(), free_head: pager.free_head() };
        let read_file = OpenOptions::new().read(true).open(path)?;
        Ok(Db {
            inner: Arc::new(Inner {
                read_file,
                shared: Mutex::new(Shared { current: Arc::new(state), readers: 0 }),
                writer: Mutex::new(Writer { pager, wal, next_txn: 1, checkpoint_pages: DEFAULT_CHECKPOINT_PAGES }),
            }),
        })
    }

    /// Change the automatic checkpoint threshold (in pages). Mainly for tests.
    pub fn set_checkpoint_pages(&self, pages: usize) {
        lock(&self.inner.writer).checkpoint_pages = pages.max(1);
    }

    /// Total pages including the header page, as of the last commit.
    pub fn page_count(&self) -> u64 {
        lock(&self.inner.shared).current.page_count
    }

    /// Current size of the log in bytes (header included).
    pub fn wal_len(&self) -> u64 {
        lock(&self.inner.writer).wal.len()
    }

    /// Start a write transaction, waiting for any other writer to finish.
    pub fn begin(&self) -> Txn<'_> {
        self.start(lock(&self.inner.writer))
    }

    /// Start a write transaction only if no other writer is active.
    pub fn try_begin(&self) -> Option<Txn<'_>> {
        match self.inner.writer.try_lock() {
            Ok(w) => Some(self.start(w)),
            Err(std::sync::TryLockError::Poisoned(e)) => Some(self.start(e.into_inner())),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    fn start<'a>(&'a self, mut w: MutexGuard<'a, Writer>) -> Txn<'a> {
        let base = lock(&self.inner.shared).current.clone();
        let id = w.next_txn;
        w.next_txn += 1;
        Txn { db: self, w, page_count: base.page_count, free_head: base.free_head, base, dirty: BTreeMap::new(), id }
    }

    /// Pin the committed state as of now. See the module docs for what that costs.
    pub fn snapshot(&self) -> Snapshot {
        let mut sh = lock(&self.inner.shared);
        sh.readers += 1;
        Snapshot { inner: self.inner.clone(), state: sh.current.clone() }
    }

    /// Read the last committed version of one page.
    ///
    /// Each call looks at the state at that moment. For a multi-page read (a tree walk) while
    /// other threads may be writing, use [`Db::snapshot`] instead.
    pub fn read(&self, id: PageId) -> Result<Page> {
        self.snapshot().read(id)
    }

    /// Copy committed pages into the main file, fsync it, then empty the log.
    /// Returns `false` (and does nothing) while any snapshot is open.
    pub fn checkpoint(&self) -> Result<bool> {
        let mut w = lock(&self.inner.writer);
        self.checkpoint_locked(&mut w)
    }

    fn checkpoint_locked(&self, w: &mut Writer) -> Result<bool> {
        // Holding `shared` for the duration also stops new snapshots from registering midway.
        let mut sh = lock(&self.inner.shared);
        if sh.readers > 0 {
            return Ok(false);
        }
        let state = sh.current.clone();
        if state.overlay.is_empty() {
            return Ok(true);
        }
        for (id, img) in &state.overlay {
            w.pager.raw_write(*id, img)?;
        }
        w.pager.sync()?;
        w.wal.reset()?;
        w.pager.refresh_header()?;
        sh.current = Arc::new(State { overlay: BTreeMap::new(), page_count: state.page_count, free_head: state.free_head });
        Ok(true)
    }

    /// Run `f` on the pager with the writer lock held and the main file fully up to date.
    /// Fails with `ReadersActive` if a snapshot prevents the checkpoint.
    pub(crate) fn with_checkpointed<T>(&self, f: impl FnOnce(&mut Pager) -> Result<T>) -> Result<T> {
        let mut w = lock(&self.inner.writer);
        if !self.checkpoint_locked(&mut w)? {
            return Err(StorageError::ReadersActive);
        }
        f(&mut w.pager)
    }

    /// Checkpoint and check every page's checksum and the free list.
    pub fn verify(&self) -> Result<Vec<StorageError>> {
        self.with_checkpointed(|p| p.verify())
    }

    /// Clean shutdown: checkpoint so the next open has nothing to replay.
    /// Fails with `ReadersActive` if snapshots are still open.
    pub fn close(self) -> Result<()> {
        match self.checkpoint()? {
            true => Ok(()),
            false => Err(StorageError::ReadersActive),
        }
    }
}

/// A frozen view of the database. Cheap to create, `Send`, never blocks the writer.
pub struct Snapshot {
    inner: Arc<Inner>,
    state: Arc<State>,
}

impl Snapshot {
    pub fn page_count(&self) -> u64 {
        self.state.page_count
    }

    pub fn read(&self, id: PageId) -> Result<Page> {
        if id == 0 {
            return Err(StorageError::InvalidPage(0));
        }
        if id >= self.state.page_count {
            return Err(StorageError::OutOfRange(id));
        }
        read_committed(&self.state, &self.inner.read_file, id)
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        lock(&self.inner.shared).readers -= 1;
    }
}

/// Anything pages can be read from: a [`Snapshot`], a [`Txn`] (its own view), or a [`Db`].
///
/// A `Db` reads the latest committed page on every call, which is only safe for multi-page
/// walks when no other thread writes; prefer [`Db::snapshot`] in concurrent code.
pub trait PageRead {
    fn read_page(&mut self, id: PageId) -> Result<Page>;
}

impl PageRead for Db {
    fn read_page(&mut self, id: PageId) -> Result<Page> {
        self.read(id)
    }
}

impl PageRead for Snapshot {
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
    db: &'a Db,
    w: MutexGuard<'a, Writer>,
    base: Arc<State>,
    dirty: BTreeMap<PageId, Box<[u8; PAGE_SIZE]>>,
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
            None => read_committed(&self.base, &self.db.inner.read_file, id),
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

    /// Make the transaction durable. When this returns `Ok`, the commit survives a crash and
    /// is visible to every snapshot taken from now on.
    pub fn commit(self) -> Result<()> {
        let Txn { db, mut w, base, mut dirty, page_count, free_head, id } = self;
        let header_changed = page_count != base.page_count || free_head != base.free_head;
        if dirty.is_empty() && !header_changed {
            return Ok(());
        }
        if header_changed {
            dirty.insert(0, Box::new(header_image(page_count, free_head)));
        }
        let images: Vec<(PageId, &[u8; PAGE_SIZE])> = dirty.iter().map(|(id, img)| (*id, &**img)).collect();
        w.wal.append_txn(id, &images)?;

        // Durable. Publish a new immutable state; existing snapshots keep the old one.
        let mut overlay = base.overlay.clone();
        for (pid, img) in dirty {
            overlay.insert(pid, Arc::from(img));
        }
        let pending = overlay.len();
        lock(&db.inner.shared).current = Arc::new(State { overlay, page_count, free_head });
        if pending >= w.checkpoint_pages {
            // `false` just means a snapshot is open; the log is simply kept for now.
            db.checkpoint_locked(&mut w).map_err(|e| StorageError::PostCommit(Box::new(e)))?;
        }
        Ok(())
    }
}
