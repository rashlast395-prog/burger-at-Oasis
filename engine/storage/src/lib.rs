//! Page-based storage for the Burger Oasis engine.
//!
//! ## What this layer guarantees
//! * The database is one file made of fixed-size pages (`PAGE_SIZE`).
//! * Every page carries a CRC-32C checksum. A page whose checksum does not match is
//!   **reported as corruption and never returned as valid data**.
//! * Page 0 is the file header (magic, format version, page size, page count, free-list head).
//! * Freed pages are reused through a free list.
//!
//! * [`Db`] adds transactions: a commit is acknowledged only after its page images and a
//!   commit record are in the write-ahead log **and fsynced**. After a crash, [`Db::open`]
//!   replays committed transactions and discards unfinished ones. [`Pager`] on its own is
//!   still not crash-atomic; use [`Db`].
//!
//! ## Known limits
//! * One writer at a time; any number of snapshot readers. Whole-page redo log (no undo, no
//!   logical records). Open snapshots postpone checkpoints, so the log grows while they live.
//! * A damaged *final* WAL record is indistinguishable from a torn write and is dropped;
//!   damage earlier in the log refuses to open.
//! * Not yet crash-tested against real power loss, only against simulated file damage.
//! * Not yet reviewed for real business data.
//!
//! ## On-disk page layout
//! ```text
//! bytes 0..4    CRC-32C (little endian) of bytes 4..PAGE_SIZE
//! byte  4       page kind (see `PageKind`)
//! bytes 5..8    reserved (zero)
//! bytes 8..     payload
//! ```

mod backup;
mod btree;
mod db;
mod page;
mod pager;
mod wal;

pub use page::{Page, PageId, PageKind, PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE, PAGE_SIZE};
pub use backup::{manifest_path_for, BackupInfo};
pub use btree::{BTree, MAX_KEY, MAX_VALUE};
pub use db::{wal_path_for, Db, PageRead, Snapshot, Txn};
pub use pager::{Pager, FORMAT_VERSION, MAGIC};

use std::fmt;
use std::io;

/// Everything that can go wrong in the storage layer. Callers must handle these;
/// nothing is silently swallowed.
#[derive(Debug)]
pub enum StorageError {
    Io(io::Error),
    /// The stored checksum does not match the page contents.
    Corrupt { page: PageId, reason: String },
    /// The file is not a Burger Oasis database, or is a different version.
    BadHeader(String),
    /// Page id is beyond the end of the file.
    OutOfRange(PageId),
    /// Attempt to use a reserved or invalid page (e.g. writing page 0 as data).
    InvalidPage(PageId),
    /// The write-ahead log is damaged in a way that is not a torn final write. Opening is
    /// refused: guessing would risk silently dropping acknowledged transactions.
    WalCorrupt(String),
    /// A backup failed verification (altered, truncated, damaged or incompatible).
    BackupInvalid(String),
    /// The operation needs the main file fully up to date, but a snapshot is still open.
    ReadersActive,
    /// A key or value exceeds the size limit (or a key is empty).
    TooLarge { what: &'static str, len: usize, max: usize },
    /// The transaction was committed durably, but the follow-up checkpoint failed.
    PostCommit(Box<StorageError>),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Io(e) => write!(f, "I/O error: {e}"),
            StorageError::Corrupt { page, reason } => write!(f, "page {page} is corrupt: {reason}"),
            StorageError::BadHeader(s) => write!(f, "invalid database header: {s}"),
            StorageError::OutOfRange(p) => write!(f, "page {p} is out of range"),
            StorageError::InvalidPage(p) => write!(f, "page {p} cannot be used for this operation"),
            StorageError::WalCorrupt(s) => write!(f, "write-ahead log is corrupt: {s}"),
            StorageError::BackupInvalid(s) => write!(f, "backup is not valid: {s}"),
            StorageError::ReadersActive => write!(f, "a snapshot is still open; drop all snapshots first"),
            StorageError::TooLarge { what, len, max } => write!(f, "{what} of {len} bytes is not allowed (limit {max}, minimum 1)"),
            StorageError::PostCommit(e) => write!(f, "transaction is durable but checkpoint failed: {e}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for StorageError {
    fn from(e: io::Error) -> Self {
        StorageError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, StorageError>;
