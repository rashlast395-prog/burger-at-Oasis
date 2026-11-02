use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::page::{Page, PageId, PageKind, PAGE_SIZE};
use crate::{Result, StorageError};

pub const MAGIC: [u8; 8] = *b"BOASISDB";
pub const FORMAT_VERSION: u32 = 1;

// Header payload layout (inside page 0's payload).
const OFF_MAGIC: usize = 0; // 8 bytes
const OFF_VERSION: usize = 8; // u32
const OFF_PAGE_SIZE: usize = 12; // u32
const OFF_PAGE_COUNT: usize = 16; // u64
const OFF_FREE_HEAD: usize = 24; // u64 (0 = empty list)

/// Reads and writes whole pages, tracks the page count and the free list.
///
/// Single-threaded for now: methods take `&mut self`. Concurrency control is a later layer.
pub struct Pager {
    file: File,
    page_count: u64,
    free_head: PageId,
}

impl Pager {
    /// Create a new database file. Fails if the file already exists.
    pub fn create(path: impl AsRef<Path>) -> Result<Pager> {
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        let mut pager = Pager { file, page_count: 1, free_head: 0 };
        pager.write_header()?;
        pager.sync()?;
        Ok(pager)
    }

    /// Open an existing database, validating the header and file length.
    pub fn open(path: impl AsRef<Path>) -> Result<Pager> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let (page_count, free_head) = load_header(&mut file)?;
        Ok(Pager { file, page_count, free_head })
    }

    pub(crate) fn free_head(&self) -> PageId {
        self.free_head
    }

    /// Total pages including the header page.
    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Read a data page. Verifies the checksum; page 0 (the header) is not readable here.
    pub fn read(&mut self, id: PageId) -> Result<Page> {
        self.check_data_page(id)?;
        self.read_unchecked(id)
    }

    /// Overwrite a page. Callers may write `Unused`, `Data` or `Index` pages only.
    pub fn write(&mut self, id: PageId, page: &Page) -> Result<()> {
        self.check_data_page(id)?;
        match page.kind() {
            PageKind::Header | PageKind::Free => return Err(StorageError::InvalidPage(id)),
            _ => {}
        }
        // Never overwrite a page that is on the free list.
        if self.read_unchecked(id)?.kind() == PageKind::Free {
            return Err(StorageError::InvalidPage(id));
        }
        write_raw(&mut self.file, id, &page.encode())
    }

    /// Reserve a page. Reuses a freed page when available, otherwise extends the file.
    /// The new page is written (with a valid checksum) before the header points at it, so a
    /// crash in between leaves at worst an unreferenced page, never a dangling reference.
    pub fn allocate(&mut self) -> Result<PageId> {
        if self.free_head != 0 {
            let id = self.free_head;
            let page = self.read_unchecked(id)?;
            if page.kind() != PageKind::Free {
                return Err(StorageError::Corrupt { page: id, reason: "free list points at a non-free page".into() });
            }
            let next = u64::from_le_bytes(page.payload()[0..8].try_into().unwrap());
            write_raw(&mut self.file, id, &Page::new(PageKind::Unused).encode())?;
            self.free_head = next;
            self.write_header()?;
            Ok(id)
        } else {
            let id = self.page_count;
            write_raw(&mut self.file, id, &Page::new(PageKind::Unused).encode())?;
            self.page_count += 1;
            self.write_header()?;
            Ok(id)
        }
    }

    /// Return a page to the free list. Freeing a page twice is rejected.
    pub fn free(&mut self, id: PageId) -> Result<()> {
        self.check_data_page(id)?;
        if self.read_unchecked(id)?.kind() == PageKind::Free {
            return Err(StorageError::InvalidPage(id));
        }
        let mut page = Page::new(PageKind::Free);
        page.payload_mut()[0..8].copy_from_slice(&self.free_head.to_le_bytes());
        write_raw(&mut self.file, id, &page.encode())?;
        self.free_head = id;
        self.write_header()
    }

    /// Force all written data to stable storage.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    /// Check every page's checksum and the free list's structure. Returns **all** problems found
    /// instead of stopping at the first one, so an operator sees the full extent of any damage.
    pub fn verify(&mut self) -> Result<Vec<StorageError>> {
        let mut problems = Vec::new();
        if let Err(e) = self.read_header_page() {
            problems.push(e);
        }
        for id in 1..self.page_count {
            if let Err(e) = self.read_unchecked(id) {
                problems.push(e);
            }
        }
        // Walk the free list: every node must be a Free page, with no cycles.
        let mut seen = std::collections::HashSet::new();
        let mut cur = self.free_head;
        while cur != 0 {
            if cur >= self.page_count || !seen.insert(cur) {
                problems.push(StorageError::Corrupt { page: cur, reason: "free list is cyclic or out of range".into() });
                break;
            }
            match self.read_unchecked(cur) {
                Ok(p) if p.kind() == PageKind::Free => {
                    cur = u64::from_le_bytes(p.payload()[0..8].try_into().unwrap());
                }
                Ok(_) => {
                    problems.push(StorageError::Corrupt { page: cur, reason: "free list contains a non-free page".into() });
                    break;
                }
                Err(_) => break, // already reported by the page scan above
            }
        }
        Ok(problems)
    }

    // ----- internals -----

    fn check_data_page(&self, id: PageId) -> Result<()> {
        if id == 0 {
            return Err(StorageError::InvalidPage(id));
        }
        if id >= self.page_count {
            return Err(StorageError::OutOfRange(id));
        }
        Ok(())
    }

    fn read_unchecked(&mut self, id: PageId) -> Result<Page> {
        let buf = read_raw(&mut self.file, id)?;
        Page::decode(id, &buf)
    }

    fn read_header_page(&mut self) -> Result<Page> {
        let buf = read_raw(&mut self.file, 0)?;
        Page::decode(0, &buf)
    }

    fn write_header(&mut self) -> Result<()> {
        write_raw(&mut self.file, 0, &header_image(self.page_count, self.free_head))
    }

    /// Write an already-encoded page image at any position, extending the file if needed.
    /// Used only by the WAL layer, which has validated the image.
    pub(crate) fn raw_write(&mut self, id: PageId, image: &[u8; PAGE_SIZE]) -> Result<()> {
        write_raw(&mut self.file, id, image)
    }

    /// Re-read page count and free list head after the WAL layer rewrote page 0.
    pub(crate) fn refresh_header(&mut self) -> Result<()> {
        let (page_count, free_head) = load_header(&mut self.file)?;
        self.page_count = page_count;
        self.free_head = free_head;
        Ok(())
    }
}

pub(crate) fn read_raw(file: &mut File, id: u64) -> Result<[u8; PAGE_SIZE]> {
    let mut buf = [0u8; PAGE_SIZE];
    file.seek(SeekFrom::Start(id * PAGE_SIZE as u64))?;
    file.read_exact(&mut buf)?;
    Ok(buf)
}

pub(crate) fn write_raw(file: &mut File, id: u64, buf: &[u8; PAGE_SIZE]) -> Result<()> {
    file.seek(SeekFrom::Start(id * PAGE_SIZE as u64))?;
    file.write_all(buf)?;
    Ok(())
}

/// Parse and validate the header page and file length of an open database file.
pub(crate) fn load_header(file: &mut File) -> Result<(u64, u64)> {
    let len = file.metadata()?.len();
    if len < PAGE_SIZE as u64 {
        return Err(StorageError::BadHeader("file is shorter than one page".into()));
    }
        let page = read_raw(file, 0)?;
        let header = Page::decode(0, &page)?;
        if header.kind() != PageKind::Header {
            return Err(StorageError::BadHeader("page 0 is not a header page".into()));
        }
        let p = header.payload();
        if p[OFF_MAGIC..OFF_MAGIC + 8] != MAGIC {
            return Err(StorageError::BadHeader("not a Burger Oasis database (bad magic)".into()));
        }
        let version = u32::from_le_bytes(p[OFF_VERSION..OFF_VERSION + 4].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(StorageError::BadHeader(format!(
                "unsupported format version {version} (this build supports {FORMAT_VERSION})"
            )));
        }
        let page_size = u32::from_le_bytes(p[OFF_PAGE_SIZE..OFF_PAGE_SIZE + 4].try_into().unwrap());
        if page_size as usize != PAGE_SIZE {
            return Err(StorageError::BadHeader(format!("page size {page_size} does not match {PAGE_SIZE}")));
        }
        let page_count = u64::from_le_bytes(p[OFF_PAGE_COUNT..OFF_PAGE_COUNT + 8].try_into().unwrap());
        let free_head = u64::from_le_bytes(p[OFF_FREE_HEAD..OFF_FREE_HEAD + 8].try_into().unwrap());
        // A file longer than page_count pages is fine (an interrupted extension leaves a tail);
        // a file shorter than the header claims means data is missing.
        if len < page_count.saturating_mul(PAGE_SIZE as u64) {
            return Err(StorageError::BadHeader(format!(
                "file is truncated: header says {page_count} pages but file holds {}",
                len / PAGE_SIZE as u64
            )));
        }
        if page_count == 0 || (free_head != 0 && free_head >= page_count) {
            return Err(StorageError::BadHeader("header contains impossible values".into()));
        }
    Ok((page_count, free_head))
}

/// The encoded bytes of the header page for the given state.
pub(crate) fn header_image(page_count: u64, free_head: PageId) -> [u8; PAGE_SIZE] {
    let mut page = Page::new(PageKind::Header);
    let p = page.payload_mut();
    p[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(&MAGIC);
    p[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    p[OFF_PAGE_SIZE..OFF_PAGE_SIZE + 4].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    p[OFF_PAGE_COUNT..OFF_PAGE_COUNT + 8].copy_from_slice(&page_count.to_le_bytes());
    p[OFF_FREE_HEAD..OFF_FREE_HEAD + 8].copy_from_slice(&free_head.to_le_bytes());
    page.encode()
}
