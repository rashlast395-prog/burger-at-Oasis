//! Consistent backups and verified restore.
//!
//! A backup is two files: the database image (`<dest>`) and a manifest (`<dest>.manifest`).
//!
//! * `Db::backup_to` checkpoints, then copies every page, **verifying each page's checksum
//!   on the way**. A corrupt source aborts the backup and leaves no files behind: a damaged
//!   database never produces a backup that looks healthy. Single writer, so nothing changes
//!   during the copy and the image is one consistent point in time.
//! * The image is written as `<dest>.partial`, fsynced, renamed; only then is the manifest
//!   written (also temp + rename). A backup without a valid manifest is not a backup.
//! * `Db::verify_backup` re-checks the manifest, file length, SHA-256, every page checksum,
//!   the free list and the header. It never modifies the backup.
//! * `Db::restore` verifies, refuses to overwrite anything, copies to a temporary name,
//!   re-hashes the copy, and only then moves it into place with a fresh empty log.
//!
//! Not provided yet: point-in-time recovery between backups, incremental backups, encryption.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::db::{wal_path_for, Db};
use crate::page::PAGE_SIZE;
use crate::pager::{Pager, FORMAT_VERSION};
use crate::wal::Wal;
use crate::{Result, StorageError};

/// What a backup contains, as recorded in its manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupInfo {
    pub page_count: u64,
    pub created_unix: u64,
    pub sha256: String,
}

fn invalid(msg: impl Into<String>) -> StorageError {
    StorageError::BackupInvalid(msg.into())
}

fn suffixed(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

pub fn manifest_path_for(backup: &Path) -> PathBuf {
    suffixed(backup, ".manifest")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn manifest_text(info: &BackupInfo) -> String {
    let body = format!(
        "BOASIS-BACKUP 1\nformat_version={FORMAT_VERSION}\npage_size={PAGE_SIZE}\npage_count={}\ncreated_unix={}\nsha256={}\n",
        info.page_count, info.created_unix, info.sha256
    );
    let crc = crc32c::crc32c(body.as_bytes());
    format!("{body}manifest_crc={crc:08x}\n")
}

fn parse_manifest(text: &str) -> Result<BackupInfo> {
    let idx = text.rfind("manifest_crc=").ok_or_else(|| invalid("manifest has no checksum line"))?;
    let (body, tail) = text.split_at(idx);
    let stored = u32::from_str_radix(tail.trim_start_matches("manifest_crc=").trim(), 16)
        .map_err(|_| invalid("manifest checksum is not valid hex"))?;
    if stored != crc32c::crc32c(body.as_bytes()) {
        return Err(invalid("manifest checksum mismatch (manifest was altered or damaged)"));
    }
    let mut lines = body.lines();
    if lines.next() != Some("BOASIS-BACKUP 1") {
        return Err(invalid("not a Burger Oasis backup manifest (or unsupported version)"));
    }
    let mut get = std::collections::HashMap::new();
    for l in lines {
        let (k, v) = l.split_once('=').ok_or_else(|| invalid("malformed manifest line"))?;
        get.insert(k.to_string(), v.to_string());
    }
    let num = |k: &str| -> Result<u64> {
        get.get(k).and_then(|v| v.parse().ok()).ok_or_else(|| invalid(format!("manifest is missing '{k}'")))
    };
    if num("format_version")? != FORMAT_VERSION as u64 || num("page_size")? != PAGE_SIZE as u64 {
        return Err(invalid("backup was made by an incompatible format version"));
    }
    let sha256 = get.get("sha256").cloned().ok_or_else(|| invalid("manifest is missing 'sha256'"))?;
    Ok(BackupInfo { page_count: num("page_count")?, created_unix: num("created_unix")?, sha256 })
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn remove_quietly(p: &Path) {
    let _ = fs::remove_file(p);
}

impl Db {
    /// Write a consistent, verified backup. Refuses to overwrite an existing backup.
    pub fn backup_to(&self, dest: impl AsRef<Path>) -> Result<BackupInfo> {
        let dest = dest.as_ref();
        let manifest = manifest_path_for(dest);
        for p in [dest, manifest.as_path()] {
            if p.exists() {
                return Err(StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("{} already exists; backups are never overwritten", p.display()),
                )));
            }
        }
        let partial = suffixed(dest, ".partial");
        remove_quietly(&partial); // leftover from an earlier interrupted backup
        let mut f = OpenOptions::new().write(true).create_new(true).open(&partial)?;
        // Holds the writer lock and requires a fully checkpointed main file (so it fails with
        // `ReadersActive` while snapshots are open) for the duration of the copy.
        let copied = self.with_checkpointed(|pager| Ok((pager.backup_copy(&mut f)?, pager.page_count())));
        let (digest, page_count) = match copied {
            Ok(d) => d,
            Err(e) => {
                drop(f);
                remove_quietly(&partial);
                return Err(e);
            }
        };
        let finish = (|| -> Result<BackupInfo> {
            f.sync_all()?;
            drop(f);
            fs::rename(&partial, dest)?;
            let info = BackupInfo {
                page_count,
                created_unix: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                sha256: hex(&digest),
            };
            let tmp = suffixed(&manifest, ".partial");
            let mut m = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
            m.write_all(manifest_text(&info).as_bytes())?;
            m.sync_all()?;
            drop(m);
            fs::rename(&tmp, &manifest)?;
            // Never report a backup that was not checked end to end.
            Db::verify_backup(dest)?;
            Ok(info)
        })();
        if finish.is_err() {
            remove_quietly(&partial);
            remove_quietly(dest);
            remove_quietly(&manifest);
            remove_quietly(&suffixed(&manifest, ".partial"));
        }
        finish
    }

    /// Check a backup end to end without modifying it.
    pub fn verify_backup(backup: impl AsRef<Path>) -> Result<BackupInfo> {
        let backup = backup.as_ref();
        let text = fs::read_to_string(manifest_path_for(backup))
            .map_err(|e| invalid(format!("cannot read the manifest: {e}")))?;
        let info = parse_manifest(&text)?;
        let len = fs::metadata(backup).map_err(|e| invalid(format!("cannot read the backup image: {e}")))?.len();
        if len != info.page_count * PAGE_SIZE as u64 {
            return Err(invalid(format!("image is {len} bytes but the manifest says {} pages", info.page_count)));
        }
        if file_sha256(backup)? != info.sha256 {
            return Err(invalid("SHA-256 of the image does not match the manifest"));
        }
        // Structural check of the image itself: header, every page checksum, free list.
        let mut pager = Pager::open(backup).map_err(|e| invalid(format!("image does not open: {e}")))?;
        if pager.page_count() != info.page_count {
            return Err(invalid("header page count disagrees with the manifest"));
        }
        let problems = pager.verify()?;
        if let Some(p) = problems.first() {
            return Err(invalid(format!("{} problem(s) inside the image, first: {p}", problems.len())));
        }
        Ok(info)
    }

    /// Restore a verified backup to `target`. Refuses to touch an existing database or log.
    pub fn restore(backup: impl AsRef<Path>, target: impl AsRef<Path>) -> Result<BackupInfo> {
        let (backup, target) = (backup.as_ref(), target.as_ref());
        let info = Db::verify_backup(backup)?;
        let wal = wal_path_for(target);
        for p in [target, wal.as_path()] {
            if p.exists() {
                return Err(StorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("{} already exists; restore never overwrites", p.display()),
                )));
            }
        }
        let tmp = suffixed(target, ".restoring");
        remove_quietly(&tmp);
        let result = (|| -> Result<()> {
            fs::copy(backup, &tmp)?;
            OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
            if file_sha256(&tmp)? != info.sha256 {
                return Err(invalid("the restored copy does not match the backup (disk or copy error)"));
            }
            Wal::create(&wal)?;
            fs::rename(&tmp, target)?;
            Ok(())
        })();
        if let Err(e) = result {
            remove_quietly(&tmp);
            remove_quietly(&wal);
            return Err(e);
        }
        Ok(info)
    }
}
