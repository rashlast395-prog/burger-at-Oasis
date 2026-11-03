use crate::{Result, StorageError};

pub const PAGE_SIZE: usize = 8192;
pub const PAGE_HEADER_SIZE: usize = 8;
pub const PAGE_PAYLOAD_SIZE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

pub type PageId = u64;

/// What a page is used for. Stored in byte 4 of every page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageKind {
    /// Never written (all zero) or released and zeroed.
    Unused = 0,
    /// File header (page 0 only).
    Header = 1,
    /// Member of the free list; payload starts with the next free page id.
    Free = 2,
    /// Record data (used by later layers).
    Data = 3,
    /// B+ tree node (used by later layers).
    Index = 4,
}

impl PageKind {
    fn from_u8(v: u8) -> Option<PageKind> {
        Some(match v {
            0 => PageKind::Unused,
            1 => PageKind::Header,
            2 => PageKind::Free,
            3 => PageKind::Data,
            4 => PageKind::Index,
            _ => return None,
        })
    }
}

/// An in-memory page image. The checksum is computed when the page is encoded for disk
/// and verified when it is decoded, so a `Page` value itself is always "logically valid".
#[derive(Clone, PartialEq, Eq)]
pub struct Page {
    kind: PageKind,
    payload: Box<[u8; PAGE_PAYLOAD_SIZE]>,
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page").field("kind", &self.kind).finish_non_exhaustive()
    }
}

impl Page {
    pub fn new(kind: PageKind) -> Page {
        Page { kind, payload: Box::new([0u8; PAGE_PAYLOAD_SIZE]) }
    }

    pub fn kind(&self) -> PageKind {
        self.kind
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload[..]
    }

    pub fn payload_mut(&mut self) -> &mut [u8] {
        &mut self.payload[..]
    }

    /// Serialise to the exact bytes written to disk, including the checksum.
    pub(crate) fn encode(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        buf[4] = self.kind as u8;
        buf[PAGE_HEADER_SIZE..].copy_from_slice(&self.payload[..]);
        let crc = crc32c::crc32c(&buf[4..]);
        buf[0..4].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Parse bytes read from disk, verifying the checksum first.
    ///
    /// There is deliberately **no** special case for all-zero pages: the pager writes every
    /// allocated page with a valid checksum straight away, so a zeroed page can only mean
    /// damage and is reported as corruption.
    pub(crate) fn decode(id: u64, buf: &[u8; PAGE_SIZE]) -> Result<Page> {
        let stored = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let actual = crc32c::crc32c(&buf[4..]);
        if stored != actual {
            return Err(StorageError::Corrupt {
                page: id,
                reason: format!("checksum mismatch (stored {stored:#010x}, computed {actual:#010x})"),
            });
        }
        let kind = PageKind::from_u8(buf[4]).ok_or_else(|| StorageError::Corrupt {
            page: id,
            reason: format!("unknown page kind {}", buf[4]),
        })?;
        let mut payload = Box::new([0u8; PAGE_PAYLOAD_SIZE]);
        payload.copy_from_slice(&buf[PAGE_HEADER_SIZE..]);
        Ok(Page { kind, payload })
    }
}
