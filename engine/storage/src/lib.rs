//! Page-based storage for the Burger Oasis engine.
//!
//! ## What this layer guarantees
//! * The database is one file made of fixed-size pages (`PAGE_SIZE`).
//! * Every page carries a CRC-32C checksum. A page whose checksum does not match is
//!   **reported as corruption and never returned as valid data**.
//! * Page 0 is the file header (magic, format version, page size, page count, free-list head).
//! * Freed pages are reused through a free list.
//!
//! ## What this layer does NOT guarantee (yet)
//! * **Crash-atomic multi-page updates.** A crash between two page writes can leave the
//!   file in a state that checksums will *detect* but this layer cannot *repair*.
//!   Atomicity and durability across pages come from the write-ahead log (next phase).
//!   Until the WAL exists this crate must not be used for real business data.
//!
//! ## On-disk page layout
//! ```text
//! bytes 0..4    CRC-32C (little endian) of bytes 4..PAGE_SIZE
//! byte  4       page kind (see `PageKind`)
//! bytes 5..8    reserved (zero)
//! bytes 8..     payload
//! ```

mod page;
mod pager;

pub use page::{Page, PageId, PageKind, PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE, PAGE_SIZE};
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
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Io(e) => write!(f, "I/O error: {e}"),
            StorageError::Corrupt { page, reason } => write!(f, "page {page} is corrupt: {reason}"),
            StorageError::BadHeader(s) => write!(f, "invalid database header: {s}"),
            StorageError::OutOfRange(p) => write!(f, "page {p} is out of range"),
            StorageError::InvalidPage(p) => write!(f, "page {p} cannot be used for this operation"),
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
