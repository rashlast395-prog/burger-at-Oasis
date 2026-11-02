//! Persistent B+ tree mapping byte-string keys to byte-string values.
//!
//! * Lives on `Index` pages inside a [`Txn`], so every change is atomic and durable through
//!   the write-ahead log.
//! * The **root page id never changes** (a split moves the old root's contents to a new
//!   page), so a catalog can store it once.
//! * Keys are compared as unsigned bytes. At most [`MAX_KEY`] bytes per key and
//!   [`MAX_VALUE`] (1 MiB) per value.
//! * Values up to `INLINE_MAX` bytes live in the leaf. Larger values are written to a chain of
//!   `Data` pages and the leaf keeps a 13-byte reference. Replacing or deleting a key frees
//!   its chain in the same transaction.
//! * Only empty nodes are reclaimed on delete; underfull-but-non-empty nodes are not merged.
//!   Space from heavy deletion is reused by later inserts but the tree does not shrink.
//!
//! Node layout (inside the page payload):
//! ```text
//! leaf:     1 | count u16 | count x ( klen u16 | vlen u16 | key | stored value )
//! stored value: 0 | bytes                       (inline)
//!               1 | total_len u32 | first u64   (overflow chain)
//! overflow page (kind Data): next u64 | chunk_len u32 | chunk bytes
//! internal: 2 | count u16 | child0 u64 | count x ( klen u16 | key | child u64 )
//! ```
//! Child `i` holds keys `k` with `keys[i-1] <= k < keys[i]`.

use std::ops::Bound;

use crate::db::{PageRead, Txn};
use crate::page::{Page, PageId, PageKind, PAGE_PAYLOAD_SIZE};
use crate::{Result, StorageError};

pub const MAX_KEY: usize = 255;
pub const MAX_VALUE: usize = 1 << 20;
/// Values longer than this go to overflow pages.
const INLINE_MAX: usize = 1500;
const CHUNK: usize = PAGE_PAYLOAD_SIZE - 12;
const TAG_INLINE: u8 = 0;
const TAG_OVERFLOW: u8 = 1;

const LEAF: u8 = 1;
const INTERNAL: u8 = 2;

type Entry = (Vec<u8>, Vec<u8>);

enum Node {
    Leaf(Vec<Entry>),
    Internal { keys: Vec<Vec<u8>>, children: Vec<PageId> },
}

impl Node {
    fn size(&self) -> usize {
        match self {
            Node::Leaf(e) => 3 + e.iter().map(|(k, v)| 4 + k.len() + v.len()).sum::<usize>(),
            Node::Internal { keys, .. } => 3 + 8 + keys.iter().map(|k| 2 + k.len() + 8).sum::<usize>(),
        }
    }

    fn fits(&self) -> bool {
        self.size() <= PAGE_PAYLOAD_SIZE
    }

    fn encode(&self) -> Page {
        let mut page = Page::new(PageKind::Index);
        let p = page.payload_mut();
        let mut at = 0usize;
        let mut put = |bytes: &[u8]| {
            p[at..at + bytes.len()].copy_from_slice(bytes);
            at += bytes.len();
        };
        match self {
            Node::Leaf(e) => {
                put(&[LEAF]);
                put(&(e.len() as u16).to_le_bytes());
                for (k, v) in e {
                    put(&(k.len() as u16).to_le_bytes());
                    put(&(v.len() as u16).to_le_bytes());
                    put(k);
                    put(v);
                }
            }
            Node::Internal { keys, children } => {
                put(&[INTERNAL]);
                put(&(keys.len() as u16).to_le_bytes());
                put(&children[0].to_le_bytes());
                for (i, k) in keys.iter().enumerate() {
                    put(&(k.len() as u16).to_le_bytes());
                    put(k);
                    put(&children[i + 1].to_le_bytes());
                }
            }
        }
        page
    }

    fn decode(id: PageId, page: &Page) -> Result<Node> {
        let bad = |why: &str| StorageError::Corrupt { page: id, reason: format!("b-tree node: {why}") };
        if page.kind() != PageKind::Index {
            return Err(bad("not an index page"));
        }
        let p = page.payload();
        let mut at = 0usize;
        let mut take = |n: usize| -> Result<&[u8]> {
            let s = p.get(at..at + n).ok_or_else(|| bad("runs past the page"))?;
            at += n;
            Ok(s)
        };
        let kind = take(1)?[0];
        let count = u16::from_le_bytes(take(2)?.try_into().unwrap()) as usize;
        match kind {
            LEAF => {
                let mut e = Vec::with_capacity(count);
                for _ in 0..count {
                    let kl = u16::from_le_bytes(take(2)?.try_into().unwrap()) as usize;
                    let vl = u16::from_le_bytes(take(2)?.try_into().unwrap()) as usize;
                    let k = take(kl)?.to_vec();
                    let v = take(vl)?.to_vec();
                    e.push((k, v));
                }
                Ok(Node::Leaf(e))
            }
            INTERNAL => {
                let mut children = Vec::with_capacity(count + 1);
                let mut keys = Vec::with_capacity(count);
                children.push(u64::from_le_bytes(take(8)?.try_into().unwrap()));
                for _ in 0..count {
                    let kl = u16::from_le_bytes(take(2)?.try_into().unwrap()) as usize;
                    keys.push(take(kl)?.to_vec());
                    children.push(u64::from_le_bytes(take(8)?.try_into().unwrap()));
                }
                Ok(Node::Internal { keys, children })
            }
            _ => Err(bad("unknown node kind")),
        }
    }
}

fn load<R: PageRead>(r: &mut R, id: PageId) -> Result<Node> {
    let page = r.read_page(id)?;
    Node::decode(id, &page)
}

fn store(txn: &mut Txn<'_>, id: PageId, node: &Node) -> Result<()> {
    txn.write(id, &node.encode())
}

fn corrupt(page: PageId, why: &str) -> StorageError {
    StorageError::Corrupt { page, reason: format!("b-tree overflow: {why}") }
}

/// Turn a user value into its stored form, writing an overflow chain if it is large.
fn encode_value(txn: &mut Txn<'_>, value: &[u8]) -> Result<Vec<u8>> {
    if value.len() <= INLINE_MAX {
        let mut out = Vec::with_capacity(1 + value.len());
        out.push(TAG_INLINE);
        out.extend_from_slice(value);
        return Ok(out);
    }
    let chunks: Vec<&[u8]> = value.chunks(CHUNK).collect();
    let mut ids = Vec::with_capacity(chunks.len());
    for _ in &chunks {
        ids.push(txn.allocate()?);
    }
    for (i, chunk) in chunks.iter().enumerate() {
        let mut page = Page::new(PageKind::Data);
        let p = page.payload_mut();
        let next = ids.get(i + 1).copied().unwrap_or(0);
        p[0..8].copy_from_slice(&next.to_le_bytes());
        p[8..12].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
        p[12..12 + chunk.len()].copy_from_slice(chunk);
        txn.write(ids[i], &page)?;
    }
    let mut out = vec![TAG_OVERFLOW];
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(&ids[0].to_le_bytes());
    Ok(out)
}

/// Walk an overflow chain, validating it, and call `visit` with each page id and chunk.
fn walk_chain<R: PageRead>(r: &mut R, total: usize, first: PageId, mut visit: impl FnMut(PageId, &[u8])) -> Result<()> {
    let pages = total.div_ceil(CHUNK);
    let mut id = first;
    let mut seen = 0usize;
    let mut bytes = 0usize;
    while id != 0 {
        if seen >= pages {
            return Err(corrupt(id, "chain is longer than the recorded length"));
        }
        let page = r.read_page(id)?;
        if page.kind() != PageKind::Data {
            return Err(corrupt(id, "chain page is not a data page"));
        }
        let p = page.payload();
        let next = u64::from_le_bytes(p[0..8].try_into().unwrap());
        let len = u32::from_le_bytes(p[8..12].try_into().unwrap()) as usize;
        if len == 0 || len > CHUNK {
            return Err(corrupt(id, "bad chunk length"));
        }
        visit(id, &p[12..12 + len]);
        bytes += len;
        seen += 1;
        id = next;
    }
    if seen != pages || bytes != total {
        return Err(corrupt(first, "chain is shorter than the recorded length"));
    }
    Ok(())
}

fn parse_ref(stored: &[u8]) -> Option<(usize, PageId)> {
    if stored.len() != 13 {
        return None;
    }
    Some((u32::from_le_bytes(stored[1..5].try_into().unwrap()) as usize, u64::from_le_bytes(stored[5..13].try_into().unwrap())))
}

/// The user-visible value for a stored value, reading the overflow chain if needed.
fn read_value<R: PageRead>(r: &mut R, page: PageId, stored: &[u8]) -> Result<Vec<u8>> {
    match stored.first() {
        Some(&TAG_INLINE) => Ok(stored[1..].to_vec()),
        Some(&TAG_OVERFLOW) => {
            let (total, first) = parse_ref(stored).ok_or_else(|| corrupt(page, "bad overflow reference"))?;
            if total <= INLINE_MAX || total > MAX_VALUE {
                return Err(corrupt(page, "overflow reference has an impossible length"));
            }
            let mut out = Vec::with_capacity(total);
            walk_chain(r, total, first, |_, chunk| out.extend_from_slice(chunk))?;
            Ok(out)
        }
        _ => Err(corrupt(page, "unknown stored value tag")),
    }
}

/// Release the overflow chain (if any) of a stored value.
fn free_value(txn: &mut Txn<'_>, page: PageId, stored: &[u8]) -> Result<()> {
    if stored.first() == Some(&TAG_OVERFLOW) {
        let (total, first) = parse_ref(stored).ok_or_else(|| corrupt(page, "bad overflow reference"))?;
        let mut ids = Vec::new();
        walk_chain(txn, total, first, |id, _| ids.push(id))?;
        for id in ids {
            txn.free(id)?;
        }
    }
    Ok(())
}

struct Split {
    sep: Vec<u8>,
    right: PageId,
}

/// Handle to a tree: just the (permanent) root page id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BTree {
    root: PageId,
}

impl BTree {
    /// Allocate an empty tree.
    pub fn create(txn: &mut Txn<'_>) -> Result<BTree> {
        let root = txn.allocate()?;
        store(txn, root, &Node::Leaf(Vec::new()))?;
        Ok(BTree { root })
    }

    pub fn open(root: PageId) -> BTree {
        BTree { root }
    }

    pub fn root(&self) -> PageId {
        self.root
    }

    pub fn get<R: PageRead>(&self, r: &mut R, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut id = self.root;
        loop {
            match load(r, id)? {
                Node::Leaf(e) => {
                    return match e.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
                        Ok(i) => Ok(Some(read_value(r, id, &e[i].1)?)),
                        Err(_) => Ok(None),
                    };
                }
                Node::Internal { keys, children } => {
                    id = children[keys.partition_point(|k| k.as_slice() <= key)];
                }
            }
        }
    }

    /// Insert or replace. Returns the previous value if the key existed.
    pub fn insert(&self, txn: &mut Txn<'_>, key: &[u8], value: &[u8]) -> Result<Option<Vec<u8>>> {
        if key.is_empty() || key.len() > MAX_KEY {
            return Err(StorageError::TooLarge { what: "key", len: key.len(), max: MAX_KEY });
        }
        if value.len() > MAX_VALUE {
            return Err(StorageError::TooLarge { what: "value", len: value.len(), max: MAX_VALUE });
        }
        let stored = encode_value(txn, value)?;
        let (old_stored, split) = insert_rec(txn, self.root, key, &stored)?;
        let old = match old_stored {
            Some(s) => {
                let v = read_value(txn, self.root, &s)?;
                free_value(txn, self.root, &s)?;
                Some(v)
            }
            None => None,
        };
        if let Some(s) = split {
            // The root page holds the left half. Move it to a new page and make the root a
            // two-child internal node, so the root id stays the same.
            let left = load(txn, self.root)?;
            let lp = txn.allocate()?;
            store(txn, lp, &left)?;
            store(txn, self.root, &Node::Internal { keys: vec![s.sep], children: vec![lp, s.right] })?;
        }
        Ok(old)
    }

    /// Remove a key. Returns its value if it existed.
    pub fn delete(&self, txn: &mut Txn<'_>, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let (old_stored, empty) = delete_rec(txn, self.root, key)?;
        let Some(stored) = old_stored else { return Ok(None) };
        let old = Some(read_value(txn, self.root, &stored)?);
        free_value(txn, self.root, &stored)?;
        if empty {
            store(txn, self.root, &Node::Leaf(Vec::new()))?;
        }
        // Collapse a root that has a single child.
        loop {
            match load(txn, self.root)? {
                Node::Internal { keys, children } if keys.is_empty() && children.len() == 1 => {
                    let child = children[0];
                    let node = load(txn, child)?;
                    store(txn, self.root, &node)?;
                    txn.free(child)?;
                }
                _ => break,
            }
        }
        Ok(old)
    }

    /// Entries with `start <= key <= end` (per the bounds), ascending, at most `limit`.
    pub fn range<R: PageRead>(
        &self,
        r: &mut R,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        limit: Option<usize>,
    ) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        scan(r, self.root, start, end, limit.unwrap_or(usize::MAX), &mut out)?;
        Ok(out)
    }

    /// Verify the tree's structure: ordering, separator bounds, child counts and uniform
    /// depth. Returns the number of entries, or the first problem found.
    pub fn check<R: PageRead>(&self, r: &mut R) -> Result<usize> {
        check_rec(r, self.root, None, None).map(|(count, _)| count)
    }
}

fn insert_rec(txn: &mut Txn<'_>, page: PageId, key: &[u8], value: &[u8]) -> Result<(Option<Vec<u8>>, Option<Split>)> {
    match load(txn, page)? {
        Node::Leaf(mut e) => {
            let old = match e.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
                Ok(i) => Some(std::mem::replace(&mut e[i].1, value.to_vec())),
                Err(i) => {
                    e.insert(i, (key.to_vec(), value.to_vec()));
                    None
                }
            };
            let node = Node::Leaf(e);
            if node.fits() {
                store(txn, page, &node)?;
                return Ok((old, None));
            }
            let Node::Leaf(mut e) = node else { unreachable!() };
            let total = Node::Leaf(e.clone()).size();
            let mut acc = 3usize;
            let mut idx = e.len() - 1;
            for (i, (k, v)) in e.iter().enumerate() {
                acc += 4 + k.len() + v.len();
                if acc * 2 >= total {
                    idx = i + 1;
                    break;
                }
            }
            let idx = idx.clamp(1, e.len() - 1);
            let right = e.split_off(idx);
            let sep = right[0].0.clone();
            let rp = txn.allocate()?;
            store(txn, page, &Node::Leaf(e))?;
            store(txn, rp, &Node::Leaf(right))?;
            Ok((old, Some(Split { sep, right: rp })))
        }
        Node::Internal { mut keys, mut children } => {
            let idx = keys.partition_point(|k| k.as_slice() <= key);
            let (old, split) = insert_rec(txn, children[idx], key, value)?;
            let Some(s) = split else { return Ok((old, None)) };
            keys.insert(idx, s.sep);
            children.insert(idx + 1, s.right);
            let node = Node::Internal { keys, children };
            if node.fits() {
                store(txn, page, &node)?;
                return Ok((old, None));
            }
            let Node::Internal { mut keys, mut children } = node else { unreachable!() };
            let mid = keys.len() / 2;
            let right_keys = keys.split_off(mid + 1);
            let sep = keys.pop().expect("internal node has keys");
            let right_children = children.split_off(mid + 1);
            let rp = txn.allocate()?;
            store(txn, page, &Node::Internal { keys, children })?;
            store(txn, rp, &Node::Internal { keys: right_keys, children: right_children })?;
            Ok((old, Some(Split { sep, right: rp })))
        }
    }
}

/// Returns (removed value, node is now empty). An empty node is NOT written back; the caller
/// frees it (or, for the root, rewrites it as an empty leaf).
fn delete_rec(txn: &mut Txn<'_>, page: PageId, key: &[u8]) -> Result<(Option<Vec<u8>>, bool)> {
    match load(txn, page)? {
        Node::Leaf(mut e) => match e.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
            Ok(i) => {
                let (_, v) = e.remove(i);
                let empty = e.is_empty();
                if !empty {
                    store(txn, page, &Node::Leaf(e))?;
                }
                Ok((Some(v), empty))
            }
            Err(_) => Ok((None, false)),
        },
        Node::Internal { mut keys, mut children } => {
            let idx = keys.partition_point(|k| k.as_slice() <= key);
            let (old, child_empty) = delete_rec(txn, children[idx], key)?;
            if old.is_none() {
                return Ok((None, false));
            }
            if child_empty {
                txn.free(children[idx])?;
                children.remove(idx);
                if !keys.is_empty() {
                    keys.remove(if idx > 0 { idx - 1 } else { 0 });
                }
                if children.is_empty() {
                    return Ok((old, true));
                }
            }
            store(txn, page, &Node::Internal { keys, children })?;
            Ok((old, false))
        }
    }
}

fn scan<R: PageRead>(
    r: &mut R,
    page: PageId,
    start: Bound<&[u8]>,
    end: Bound<&[u8]>,
    limit: usize,
    out: &mut Vec<Entry>,
) -> Result<()> {
    match load(r, page)? {
        Node::Leaf(e) => {
            for (k, stored) in e {
                if out.len() >= limit {
                    break;
                }
                let after_start = match start {
                    Bound::Included(s) => k.as_slice() >= s,
                    Bound::Excluded(s) => k.as_slice() > s,
                    Bound::Unbounded => true,
                };
                let before_end = match end {
                    Bound::Included(s) => k.as_slice() <= s,
                    Bound::Excluded(s) => k.as_slice() < s,
                    Bound::Unbounded => true,
                };
                if after_start && before_end {
                    let v = read_value(r, page, &stored)?;
                    out.push((k, v));
                }
            }
        }
        Node::Internal { keys, children } => {
            let lo = match start {
                Bound::Included(s) | Bound::Excluded(s) => keys.partition_point(|k| k.as_slice() <= s),
                Bound::Unbounded => 0,
            };
            let hi = match end {
                Bound::Included(s) | Bound::Excluded(s) => keys.partition_point(|k| k.as_slice() <= s),
                Bound::Unbounded => keys.len(),
            };
            for c in children.iter().take(hi + 1).skip(lo) {
                if out.len() >= limit {
                    break;
                }
                scan(r, *c, start, end, limit, out)?;
            }
        }
    }
    Ok(())
}

fn check_rec<R: PageRead>(r: &mut R, page: PageId, lo: Option<&[u8]>, hi: Option<&[u8]>) -> Result<(usize, usize)> {
    let bad = |why: String| StorageError::Corrupt { page, reason: format!("b-tree: {why}") };
    let in_bounds = |k: &[u8]| lo.is_none_or(|l| k >= l) && hi.is_none_or(|h| k < h);
    match load(r, page)? {
        Node::Leaf(e) => {
            for w in e.windows(2) {
                if w[0].0 >= w[1].0 {
                    return Err(bad("leaf keys are not strictly ascending".into()));
                }
            }
            if e.iter().any(|(k, _)| !in_bounds(k)) {
                return Err(bad("leaf key outside its separator bounds".into()));
            }
            for (_, stored) in &e {
                read_value(r, page, stored)?; // validates inline tags and whole overflow chains
            }
            Ok((e.len(), 1))
        }
        Node::Internal { keys, children } => {
            if children.len() != keys.len() + 1 {
                return Err(bad("child count does not match key count".into()));
            }
            for w in keys.windows(2) {
                if w[0] >= w[1] {
                    return Err(bad("separator keys are not strictly ascending".into()));
                }
            }
            if keys.iter().any(|k| !in_bounds(k)) {
                return Err(bad("separator outside its parent's bounds".into()));
            }
            let mut total = 0;
            let mut depth = None;
            for (i, c) in children.iter().enumerate() {
                let clo = if i == 0 { lo } else { Some(keys[i - 1].as_slice()) };
                let chi = if i == keys.len() { hi } else { Some(keys[i].as_slice()) };
                let (n, d) = check_rec(r, *c, clo, chi)?;
                total += n;
                match depth {
                    None => depth = Some(d),
                    Some(prev) if prev != d => return Err(bad("leaves are at different depths".into())),
                    _ => {}
                }
            }
            Ok((total, depth.unwrap_or(0) + 1))
        }
    }
}
