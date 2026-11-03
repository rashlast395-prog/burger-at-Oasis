//! Write-ahead log: a redo log of whole page images.
//!
//! ```text
//! file header (16 bytes): magic "BOASWAL1" | version u32 | crc32c of the first 12 bytes
//! record: crc32c u32 | payload_len u32 | kind u8 | txn_id u64 | payload
//!         (crc covers everything after the crc field)
//!   kind 1 PAGE   payload = page_id u64 | encoded page image (PAGE_SIZE bytes)
//!   kind 2 COMMIT payload = number of PAGE records in this transaction (u32)
//! ```
//! A transaction counts as committed only when its COMMIT record is present and valid.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::page::{PageId, PAGE_SIZE};
use crate::{Result, StorageError};

const MAGIC: [u8; 8] = *b"BOASWAL1";
const VERSION: u32 = 1;
const FILE_HEADER: u64 = 16;
const REC_HEAD: usize = 17;
const REC_PAGE: u8 = 1;
const REC_COMMIT: u8 = 2;
const PAGE_PAYLOAD: usize = 8 + PAGE_SIZE;

pub(crate) type Image = Box<[u8; PAGE_SIZE]>;

/// One committed transaction read back from the log.
pub(crate) struct Committed {
    pub pages: Vec<(PageId, Image)>,
}

pub(crate) struct Wal {
    file: File,
    end: u64,
}

fn file_header() -> [u8; FILE_HEADER as usize] {
    let mut h = [0u8; FILE_HEADER as usize];
    h[0..8].copy_from_slice(&MAGIC);
    h[8..12].copy_from_slice(&VERSION.to_le_bytes());
    let crc = crc32c::crc32c(&h[0..12]);
    h[12..16].copy_from_slice(&crc.to_le_bytes());
    h
}

fn encode_record(kind: u8, txn_id: u64, payload: &[u8], out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&txn_id.to_le_bytes());
    out.extend_from_slice(payload);
    let crc = crc32c::crc32c(&out[start + 4..]);
    out[start..start + 4].copy_from_slice(&crc.to_le_bytes());
}

impl Wal {
    pub fn create(path: impl AsRef<Path>) -> Result<Wal> {
        let mut file = OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        file.write_all(&file_header())?;
        file.sync_all()?;
        Ok(Wal { file, end: FILE_HEADER })
    }

    /// Open, read every committed transaction, and cut off a torn tail.
    pub fn open(path: impl AsRef<Path>) -> Result<(Wal, Vec<Committed>)> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        if data.len() < FILE_HEADER as usize {
            return Err(StorageError::WalCorrupt("file is shorter than its header".into()));
        }
        let h = &data[..FILE_HEADER as usize];
        if h[0..8] != MAGIC {
            return Err(StorageError::WalCorrupt("bad magic".into()));
        }
        let stored = u32::from_le_bytes(h[12..16].try_into().unwrap());
        if stored != crc32c::crc32c(&h[0..12]) {
            return Err(StorageError::WalCorrupt("header checksum mismatch".into()));
        }
        let version = u32::from_le_bytes(h[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(StorageError::WalCorrupt(format!("unsupported version {version}")));
        }

        let mut committed = Vec::new();
        let mut pending: Vec<(PageId, Image)> = Vec::new();
        let mut pending_txn: Option<u64> = None;
        let mut pos = FILE_HEADER as usize;
        let mut valid_end = pos;
        while pos < data.len() {
            if data.len() - pos < REC_HEAD {
                break; // torn tail
            }
            let plen = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
            if plen > PAGE_PAYLOAD || pos + REC_HEAD + plen > data.len() {
                break; // length is nonsense or runs past the end: torn tail
            }
            let total = REC_HEAD + plen;
            let rec = &data[pos..pos + total];
            let stored = u32::from_le_bytes(rec[0..4].try_into().unwrap());
            if stored != crc32c::crc32c(&rec[4..]) {
                if pos + total < data.len() {
                    return Err(StorageError::WalCorrupt(format!(
                        "record at offset {pos} fails its checksum but more log follows it"
                    )));
                }
                break; // damaged final record: treated as a torn write
            }
            let kind = rec[8];
            let txn_id = u64::from_le_bytes(rec[9..17].try_into().unwrap());
            let payload = &rec[REC_HEAD..];
            match kind {
                REC_PAGE => {
                    if payload.len() != PAGE_PAYLOAD {
                        return Err(StorageError::WalCorrupt(format!("page record at {pos} has wrong size")));
                    }
                    if pending_txn != Some(txn_id) {
                        pending.clear(); // an earlier transaction never committed
                        pending_txn = Some(txn_id);
                    }
                    let id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                    let mut img = Box::new([0u8; PAGE_SIZE]);
                    img.copy_from_slice(&payload[8..]);
                    pending.push((id, img));
                }
                REC_COMMIT => {
                    if payload.len() != 4 {
                        return Err(StorageError::WalCorrupt(format!("commit record at {pos} has wrong size")));
                    }
                    let n = u32::from_le_bytes(payload.try_into().unwrap()) as usize;
                    if pending_txn != Some(txn_id) || pending.len() != n {
                        return Err(StorageError::WalCorrupt(format!(
                            "commit record at {pos} does not match the records before it"
                        )));
                    }
                    committed.push(Committed { pages: std::mem::take(&mut pending) });
                    pending_txn = None;
                    valid_end = pos + total;
                }
                k => return Err(StorageError::WalCorrupt(format!("unknown record kind {k} at {pos}"))),
            }
            pos += total;
        }

        // Cut off anything after the last commit (torn or uncommitted records).
        if valid_end < data.len() {
            file.set_len(valid_end as u64)?;
            file.sync_all()?;
        }
        Ok((Wal { file, end: valid_end as u64 }, committed))
    }

    /// Append one transaction and force it to stable storage. Returns only after fsync.
    pub fn append_txn(&mut self, txn_id: u64, pages: &[(PageId, &[u8; PAGE_SIZE])]) -> Result<()> {
        let mut buf = Vec::with_capacity(pages.len() * (REC_HEAD + PAGE_PAYLOAD) + REC_HEAD + 4);
        let mut payload = Vec::with_capacity(PAGE_PAYLOAD);
        for (id, img) in pages {
            payload.clear();
            payload.extend_from_slice(&id.to_le_bytes());
            payload.extend_from_slice(&img[..]);
            encode_record(REC_PAGE, txn_id, &payload, &mut buf);
        }
        encode_record(REC_COMMIT, txn_id, &(pages.len() as u32).to_le_bytes(), &mut buf);

        let result = (|| -> Result<()> {
            self.file.seek(SeekFrom::Start(self.end))?;
            self.file.write_all(&buf)?;
            self.file.sync_all()?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.end += buf.len() as u64;
                Ok(())
            }
            Err(e) => {
                // Best effort: remove the partial transaction so later commits start clean.
                let _ = self.file.set_len(self.end);
                Err(e)
            }
        }
    }

    /// Discard the whole log (after its contents are safely in the main file).
    pub fn reset(&mut self) -> Result<()> {
        self.file.set_len(FILE_HEADER)?;
        self.file.sync_all()?;
        self.end = FILE_HEADER;
        Ok(())
    }

    pub fn len(&self) -> u64 {
        self.end
    }
}
