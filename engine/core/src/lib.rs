//! Collections of JSON documents, secondary indexes and a structured query language,
//! built on the transactional B+ trees of `bo-storage`.
//!
//! ## Model
//! * A **catalog** B+ tree (always at page 1) maps collection name -> metadata.
//! * Each collection has a **primary tree**: `_id` (string) -> document JSON.
//! * Each **index** is a tree of `encode(value) 0x00 _id` -> empty, so entries sort by value
//!   then id. Documents lacking the field are simply not in that index.
//! * Everything runs inside [`Tx`], i.e. one storage transaction: a document write and all
//!   of its index updates commit or roll back together.
//!
//! ## Query language (JSON, no SQL)
//! ```json
//! { "filter": { "status": "open", "total": { "$gte": 10, "$lt": 50 },
//!               "$or": [ { "channel": "WEB" }, { "channel": "POS" } ] },
//!   "sort": { "field": "total", "dir": "desc" }, "skip": 0, "limit": 20 }
//! ```
//! Operators: `$eq $ne $gt $gte $lt $lte $in $nin $exists`, combinators `$and $or $not`.
//! Field paths may be dotted (`"customer.phone"`). Range operators only match values of the
//! same type (numbers with numbers, strings with strings).
//!
//! ## Limits (documented, enforced)
//! * A serialized document may be at most [`MAX_DOC_BYTES`]; there are no overflow pages yet.
//! * Only scalar values (null, bool, number, string) can be indexed; strings containing NUL
//!   cannot; an index key (value + id) must fit the B+ tree key limit.
//! * Results are materialised in memory; sorting is done in memory after filtering.
//!   Without a `sort`, results are ordered by `_id`; sorts break ties by `_id`.
//! * The planner uses at most one index per query and always re-checks the full filter.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;
use std::ops::Bound;
use std::path::Path;

use bo_storage::{BTree, Db, PageRead, StorageError, Txn, MAX_KEY, MAX_VALUE};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const MAX_DOC_BYTES: usize = MAX_VALUE;
const MAX_INDEXES: usize = 16;
const MAX_ID: usize = 100;

#[derive(Debug)]
pub enum EngineError {
    Storage(StorageError),
    Json(String),
    NoSuchCollection(String),
    CollectionExists(String),
    IndexExists(String),
    DuplicateId(String),
    NotFound(String),
    UniqueViolation { index: String },
    InvalidName(String),
    InvalidDocument(String),
    InvalidQuery(String),
    DocumentTooLarge { len: usize, max: usize },
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use EngineError::*;
        match self {
            Storage(e) => write!(f, "storage error: {e}"),
            Json(e) => write!(f, "json error: {e}"),
            NoSuchCollection(n) => write!(f, "collection '{n}' does not exist"),
            CollectionExists(n) => write!(f, "collection '{n}' already exists"),
            IndexExists(n) => write!(f, "index on '{n}' already exists"),
            DuplicateId(id) => write!(f, "a document with _id '{id}' already exists"),
            NotFound(id) => write!(f, "no document with _id '{id}'"),
            UniqueViolation { index } => write!(f, "unique index on '{index}' would be violated"),
            InvalidName(n) => write!(f, "invalid name '{n}'"),
            InvalidDocument(m) => write!(f, "invalid document: {m}"),
            InvalidQuery(m) => write!(f, "invalid query: {m}"),
            DocumentTooLarge { len, max } => write!(f, "document of {len} bytes exceeds the {max} byte limit"),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<StorageError> for EngineError {
    fn from(e: StorageError) -> Self {
        EngineError::Storage(e)
    }
}

impl From<serde_json::Error> for EngineError {
    fn from(e: serde_json::Error) -> Self {
        EngineError::Json(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Serialize, Deserialize, Clone)]
struct IndexMeta {
    field: String,
    unique: bool,
    root: u64,
}

#[derive(Serialize, Deserialize, Clone)]
struct Meta {
    primary: u64,
    indexes: Vec<IndexMeta>,
}

// ---------------------------------------------------------------- engine

pub struct Engine {
    db: Db,
    catalog: BTree,
}

impl Engine {
    pub fn create(path: impl AsRef<Path>) -> Result<Engine> {
        let mut db = Db::create(path)?;
        let mut t = db.begin();
        let catalog = BTree::create(&mut t)?;
        debug_assert_eq!(catalog.root(), 1);
        t.commit()?;
        Ok(Engine { db, catalog })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Engine> {
        let mut db = Db::open(path)?;
        let catalog = BTree::open(1);
        catalog.check(&mut db)?; // refuses a database whose catalog is damaged
        Ok(Engine { db, catalog })
    }

    pub fn begin(&mut self) -> Tx<'_> {
        Tx { txn: self.db.begin(), catalog: self.catalog }
    }

    pub fn get(&mut self, collection: &str, id: &str) -> Result<Option<Value>> {
        get_doc(&mut self.db, &self.catalog, collection, id)
    }

    pub fn find(&mut self, collection: &str, query: &Value) -> Result<Vec<Value>> {
        find_docs(&mut self.db, &self.catalog, collection, query)
    }

    pub fn explain(&mut self, collection: &str, query: &Value) -> Result<String> {
        explain_query(&mut self.db, &self.catalog, collection, query)
    }

    pub fn collections(&mut self) -> Result<Vec<String>> {
        list_collections(&mut self.db, &self.catalog)
    }

    /// Check every tree of a collection and that each index matches its documents exactly.
    /// Returns a list of problems (empty = healthy).
    pub fn verify(&mut self, collection: &str) -> Result<Vec<String>> {
        verify_collection(&mut self.db, &self.catalog, collection)
    }

    /// Clean shutdown (checkpoints the log).
    pub fn close(self) -> Result<()> {
        Ok(self.db.close()?)
    }

    /// Give the storage layer back, e.g. to simulate a crash by dropping without `close`.
    pub fn into_db(self) -> Db {
        self.db
    }
}

pub struct Tx<'a> {
    txn: Txn<'a>,
    catalog: BTree,
}

impl Tx<'_> {
    pub fn create_collection(&mut self, name: &str) -> Result<()> {
        check_name(name)?;
        if self.catalog.get(&mut self.txn, name.as_bytes())?.is_some() {
            return Err(EngineError::CollectionExists(name.into()));
        }
        let primary = BTree::create(&mut self.txn)?;
        save_meta(&mut self.txn, &self.catalog, name, &Meta { primary: primary.root(), indexes: vec![] })
    }

    /// Add an index on `field` (dotted path) and build it from the existing documents.
    pub fn create_index(&mut self, collection: &str, field: &str, unique: bool) -> Result<()> {
        check_field(field)?;
        let mut meta = load_meta(&mut self.txn, &self.catalog, collection)?;
        if meta.indexes.iter().any(|i| i.field == field) {
            return Err(EngineError::IndexExists(field.into()));
        }
        if meta.indexes.len() >= MAX_INDEXES {
            return Err(EngineError::InvalidName(format!("more than {MAX_INDEXES} indexes")));
        }
        let tree = BTree::create(&mut self.txn)?;
        let primary = BTree::open(meta.primary);
        for (k, v) in primary.range(&mut self.txn, Bound::Unbounded, Bound::Unbounded, None)? {
            let id = String::from_utf8_lossy(&k).into_owned();
            let doc: Value = serde_json::from_slice(&v)?;
            if let Some(key) = field_key(&doc, field, &id)? {
                if unique && other_with_same_value(&mut self.txn, &tree, &key, &id)? {
                    return Err(EngineError::UniqueViolation { index: field.into() });
                }
                tree.insert(&mut self.txn, &key, &[])?;
            }
        }
        meta.indexes.push(IndexMeta { field: field.into(), unique, root: tree.root() });
        save_meta(&mut self.txn, &self.catalog, collection, &meta)
    }

    /// Insert a new document (must contain a string `_id`). Fails if the id exists.
    pub fn insert(&mut self, collection: &str, doc: Value) -> Result<()> {
        self.put(collection, doc, true)
    }

    /// Insert or fully replace the document with this `_id`.
    pub fn upsert(&mut self, collection: &str, doc: Value) -> Result<()> {
        self.put(collection, doc, false)
    }

    /// Shallow-merge `set` (an object) into an existing document. `_id` cannot change.
    pub fn update(&mut self, collection: &str, id: &str, set: &Value) -> Result<()> {
        let Value::Object(set) = set else {
            return Err(EngineError::InvalidDocument("update must be an object".into()));
        };
        if set.get("_id").is_some_and(|v| v != &Value::String(id.into())) {
            return Err(EngineError::InvalidDocument("_id cannot be changed".into()));
        }
        let Some(Value::Object(mut doc)) = get_doc(&mut self.txn, &self.catalog, collection, id)? else {
            return Err(EngineError::NotFound(id.into()));
        };
        for (k, v) in set {
            doc.insert(k.clone(), v.clone());
        }
        self.put(collection, Value::Object(doc), false)
    }

    pub fn delete(&mut self, collection: &str, id: &str) -> Result<()> {
        let meta = load_meta(&mut self.txn, &self.catalog, collection)?;
        let primary = BTree::open(meta.primary);
        let Some(bytes) = primary.get(&mut self.txn, id.as_bytes())? else {
            return Err(EngineError::NotFound(id.into()));
        };
        let doc: Value = serde_json::from_slice(&bytes)?;
        for im in &meta.indexes {
            if let Some(key) = field_key(&doc, &im.field, id)? {
                BTree::open(im.root).delete(&mut self.txn, &key)?;
            }
        }
        primary.delete(&mut self.txn, id.as_bytes())?;
        Ok(())
    }

    pub fn get(&mut self, collection: &str, id: &str) -> Result<Option<Value>> {
        get_doc(&mut self.txn, &self.catalog, collection, id)
    }

    pub fn find(&mut self, collection: &str, query: &Value) -> Result<Vec<Value>> {
        find_docs(&mut self.txn, &self.catalog, collection, query)
    }

    pub fn explain(&mut self, collection: &str, query: &Value) -> Result<String> {
        explain_query(&mut self.txn, &self.catalog, collection, query)
    }

    /// Make everything in this transaction durable. Dropping a `Tx` instead rolls it back.
    pub fn commit(self) -> Result<()> {
        Ok(self.txn.commit()?)
    }

    fn put(&mut self, collection: &str, doc: Value, must_be_new: bool) -> Result<()> {
        let meta = load_meta(&mut self.txn, &self.catalog, collection)?;
        let id = doc_id(&doc)?;
        let bytes = serde_json::to_vec(&doc)?;
        if bytes.len() > MAX_DOC_BYTES {
            return Err(EngineError::DocumentTooLarge { len: bytes.len(), max: MAX_DOC_BYTES });
        }
        let primary = BTree::open(meta.primary);
        let old: Option<Value> = match primary.get(&mut self.txn, id.as_bytes())? {
            Some(b) => {
                if must_be_new {
                    return Err(EngineError::DuplicateId(id));
                }
                Some(serde_json::from_slice(&b)?)
            }
            None => None,
        };

        // Plan every index change and run every check BEFORE mutating anything, so a failure
        // leaves the transaction exactly as it was.
        let mut changes: Vec<(BTree, Option<Vec<u8>>, Option<Vec<u8>>)> = Vec::new();
        for im in &meta.indexes {
            let tree = BTree::open(im.root);
            let new_key = field_key(&doc, &im.field, &id)?;
            let old_key = match &old {
                Some(d) => field_key(d, &im.field, &id)?,
                None => None,
            };
            if new_key == old_key {
                continue;
            }
            if let (true, Some(k)) = (im.unique, &new_key) {
                if other_with_same_value(&mut self.txn, &tree, k, &id)? {
                    return Err(EngineError::UniqueViolation { index: im.field.clone() });
                }
            }
            changes.push((tree, old_key, new_key));
        }
        for (tree, old_key, new_key) in changes {
            if let Some(k) = old_key {
                tree.delete(&mut self.txn, &k)?;
            }
            if let Some(k) = new_key {
                tree.insert(&mut self.txn, &k, &[])?;
            }
        }
        primary.insert(&mut self.txn, id.as_bytes(), &bytes)?;
        Ok(())
    }
}

// ---------------------------------------------------------------- helpers

fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(EngineError::InvalidName(name.into()))
    }
}

fn check_field(field: &str) -> Result<()> {
    let ok = !field.is_empty() && field.len() <= 64 && field != "_id" && field.split('.').all(|s| !s.is_empty());
    if ok {
        Ok(())
    } else {
        Err(EngineError::InvalidName(field.into()))
    }
}

fn doc_id(doc: &Value) -> Result<String> {
    let Value::Object(m) = doc else {
        return Err(EngineError::InvalidDocument("a document must be a JSON object".into()));
    };
    match m.get("_id") {
        Some(Value::String(s)) if !s.is_empty() && s.len() <= MAX_ID && !s.contains('\0') => Ok(s.clone()),
        _ => Err(EngineError::InvalidDocument(format!("_id must be a non-empty string (max {MAX_ID} bytes, no NUL)"))),
    }
}

fn load_meta<R: PageRead>(r: &mut R, catalog: &BTree, name: &str) -> Result<Meta> {
    match catalog.get(r, name.as_bytes())? {
        Some(b) => Ok(serde_json::from_slice(&b)?),
        None => Err(EngineError::NoSuchCollection(name.into())),
    }
}

fn save_meta(txn: &mut Txn<'_>, catalog: &BTree, name: &str, meta: &Meta) -> Result<()> {
    catalog.insert(txn, name.as_bytes(), &serde_json::to_vec(meta)?)?;
    Ok(())
}

fn list_collections<R: PageRead>(r: &mut R, catalog: &BTree) -> Result<Vec<String>> {
    let all = catalog.range(r, Bound::Unbounded, Bound::Unbounded, None)?;
    Ok(all.into_iter().map(|(k, _)| String::from_utf8_lossy(&k).into_owned()).collect())
}

fn get_doc<R: PageRead>(r: &mut R, catalog: &BTree, collection: &str, id: &str) -> Result<Option<Value>> {
    let meta = load_meta(r, catalog, collection)?;
    match BTree::open(meta.primary).get(r, id.as_bytes())? {
        Some(b) => Ok(Some(serde_json::from_slice(&b)?)),
        None => Ok(None),
    }
}

fn get_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = doc;
    for part in path.split('.') {
        cur = cur.as_object()?.get(part)?;
    }
    Some(cur)
}

/// Order-preserving encoding of a scalar. Type tag first, so values of different types
/// never interleave: null 1, false 2, true 3, number 4, string 5.
fn enc_scalar(v: &Value) -> Result<Vec<u8>> {
    Ok(match v {
        Value::Null => vec![1],
        Value::Bool(false) => vec![2],
        Value::Bool(true) => vec![3],
        Value::Number(n) => {
            let f = n.as_f64().ok_or_else(|| EngineError::InvalidDocument("number is not representable".into()))?;
            let f = if f == 0.0 { 0.0 } else { f }; // fold -0 into 0
            let bits = f.to_bits();
            let ordered = if bits >> 63 == 1 { !bits } else { bits | (1 << 63) };
            let mut o = vec![4];
            o.extend_from_slice(&ordered.to_be_bytes());
            o
        }
        Value::String(s) => {
            if s.as_bytes().contains(&0) {
                return Err(EngineError::InvalidDocument("indexed strings cannot contain NUL".into()));
            }
            let mut o = vec![5];
            o.extend_from_slice(s.as_bytes());
            o
        }
        _ => return Err(EngineError::InvalidDocument("only scalar values can be indexed".into())),
    })
}

/// Index entry key for this document, or `None` if the field is absent.
fn field_key(doc: &Value, field: &str, id: &str) -> Result<Option<Vec<u8>>> {
    let Some(v) = get_path(doc, field) else { return Ok(None) };
    let mut key = enc_scalar(v).map_err(|e| match e {
        EngineError::InvalidDocument(m) => EngineError::InvalidDocument(format!("field '{field}': {m}")),
        other => other,
    })?;
    key.push(0);
    key.extend_from_slice(id.as_bytes());
    if key.len() > MAX_KEY {
        return Err(EngineError::InvalidDocument(format!("field '{field}': indexed value is too long")));
    }
    Ok(Some(key))
}

fn id_of_entry(key: &[u8]) -> String {
    // ids contain no NUL, so the last NUL separates value from id
    let pos = key.iter().rposition(|b| *b == 0).map(|p| p + 1).unwrap_or(0);
    String::from_utf8_lossy(&key[pos..]).into_owned()
}

/// Does the unique index already hold this value for a *different* document?
fn other_with_same_value(txn: &mut Txn<'_>, tree: &BTree, key: &[u8], id: &str) -> Result<bool> {
    let prefix = &key[..key.len() - id.len()]; // enc(value) 0x00
    let mut hi = prefix.to_vec();
    *hi.last_mut().unwrap() = 1;
    let found = tree.range(txn, Bound::Included(prefix), Bound::Excluded(&hi[..]), Some(2))?;
    Ok(found.iter().any(|(k, _)| id_of_entry(k) != id))
}

// ---------------------------------------------------------------- query

struct Query {
    filter: Value,
    sort: Option<(String, bool)>,
    skip: usize,
    limit: Option<usize>,
}

fn bad(msg: impl Into<String>) -> EngineError {
    EngineError::InvalidQuery(msg.into())
}

fn parse_query(q: &Value) -> Result<Query> {
    let Value::Object(m) = q else { return Err(bad("query must be an object")) };
    let mut out = Query { filter: Value::Object(Map::new()), sort: None, skip: 0, limit: None };
    for (k, v) in m {
        match k.as_str() {
            "filter" => {
                validate_filter(v)?;
                out.filter = v.clone();
            }
            "sort" => {
                let Value::Object(s) = v else { return Err(bad("sort must be an object")) };
                let field = s.get("field").and_then(Value::as_str).ok_or_else(|| bad("sort.field must be a string"))?;
                let asc = match s.get("dir").and_then(Value::as_str) {
                    None | Some("asc") => true,
                    Some("desc") => false,
                    _ => return Err(bad("sort.dir must be \"asc\" or \"desc\"")),
                };
                if s.keys().any(|k| k != "field" && k != "dir") {
                    return Err(bad("unknown key in sort"));
                }
                out.sort = Some((field.to_string(), asc));
            }
            "skip" => out.skip = v.as_u64().ok_or_else(|| bad("skip must be a non-negative integer"))? as usize,
            "limit" => out.limit = Some(v.as_u64().ok_or_else(|| bad("limit must be a non-negative integer"))? as usize),
            other => return Err(bad(format!("unknown query key '{other}'"))),
        }
    }
    Ok(out)
}

fn is_op_object(v: &Value) -> bool {
    matches!(v, Value::Object(m) if !m.is_empty() && m.keys().all(|k| k.starts_with('$')))
}

fn validate_filter(f: &Value) -> Result<()> {
    let Value::Object(m) = f else { return Err(bad("filter must be an object")) };
    for (k, v) in m {
        match k.as_str() {
            "$and" | "$or" => {
                let Value::Array(items) = v else { return Err(bad(format!("{k} must be an array"))) };
                if items.is_empty() {
                    return Err(bad(format!("{k} must not be empty")));
                }
                items.iter().try_for_each(validate_filter)?;
            }
            "$not" => validate_filter(v)?,
            k if k.starts_with('$') => return Err(bad(format!("unknown operator '{k}' at top level"))),
            _ => {
                if let Value::Object(ops) = v {
                    if ops.keys().any(|k| k.starts_with('$')) && !is_op_object(v) {
                        return Err(bad(format!("field '{k}' mixes operators and plain keys")));
                    }
                    for (op, arg) in ops.iter().filter(|(o, _)| o.starts_with('$')) {
                        match op.as_str() {
                            "$eq" | "$ne" | "$gt" | "$gte" | "$lt" | "$lte" => {}
                            "$in" | "$nin" if arg.is_array() => {}
                            "$exists" if arg.is_boolean() => {}
                            _ => return Err(bad(format!("bad operator or argument '{op}' on field '{k}'"))),
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.as_bytes().cmp(y.as_bytes())),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn matches(doc: &Value, filter: &Value) -> bool {
    let Value::Object(m) = filter else { return false };
    m.iter().all(|(k, v)| match k.as_str() {
        "$and" => v.as_array().is_some_and(|a| a.iter().all(|f| matches(doc, f))),
        "$or" => v.as_array().is_some_and(|a| a.iter().any(|f| matches(doc, f))),
        "$not" => !matches(doc, v),
        field => {
            let val = get_path(doc, field);
            if is_op_object(v) {
                v.as_object().unwrap().iter().all(|(op, arg)| op_matches(val, op, arg))
            } else {
                val.is_some_and(|x| values_equal(x, v))
            }
        }
    })
}

fn op_matches(val: Option<&Value>, op: &str, arg: &Value) -> bool {
    match op {
        "$exists" => val.is_some() == arg.as_bool().unwrap_or(false),
        "$eq" => val.is_some_and(|x| values_equal(x, arg)),
        "$ne" => !val.is_some_and(|x| values_equal(x, arg)),
        "$in" => val.is_some_and(|x| arg.as_array().is_some_and(|a| a.iter().any(|c| values_equal(x, c)))),
        "$nin" => !val.is_some_and(|x| arg.as_array().is_some_and(|a| a.iter().any(|c| values_equal(x, c)))),
        "$gt" => val.and_then(|x| compare(x, arg)) == Some(Ordering::Greater),
        "$gte" => matches!(val.and_then(|x| compare(x, arg)), Some(Ordering::Greater | Ordering::Equal)),
        "$lt" => val.and_then(|x| compare(x, arg)) == Some(Ordering::Less),
        "$lte" => matches!(val.and_then(|x| compare(x, arg)), Some(Ordering::Less | Ordering::Equal)),
        _ => false,
    }
}

enum Plan {
    Full,
    Index { pos: usize, lo: Bound<Vec<u8>>, hi: Bound<Vec<u8>>, kind: &'static str },
}

fn plan_query(meta: &Meta, filter: &Value) -> Plan {
    let Value::Object(m) = filter else { return Plan::Full };
    let mut best: Option<(u8, Plan)> = None;
    for (field, cond) in m.iter().filter(|(k, _)| !k.starts_with('$')) {
        let Some(pos) = meta.indexes.iter().position(|i| &i.field == field) else { continue };
        let unique = meta.indexes[pos].unique;
        let ops: Vec<(&str, &Value)> = if is_op_object(cond) {
            cond.as_object().unwrap().iter().map(|(o, a)| (o.as_str(), a)).collect()
        } else {
            vec![("$eq", cond)]
        };
        // equality
        if let Some((_, lit)) = ops.iter().find(|(o, _)| *o == "$eq") {
            if let Ok(e) = enc_scalar(lit) {
                let mut lo = e.clone();
                lo.push(0);
                let mut hi = e;
                hi.push(1);
                let score = if unique { 3 } else { 2 };
                if best.as_ref().is_none_or(|(s, _)| score > *s) {
                    best = Some((score, Plan::Index { pos, lo: Bound::Included(lo), hi: Bound::Excluded(hi), kind: "equality" }));
                }
                continue;
            }
        }
        // range on numbers or strings
        let mut tag = None;
        let mut lo = None;
        let mut hi = None;
        for (op, lit) in &ops {
            let t = match lit {
                Value::Number(_) => 4u8,
                Value::String(_) => 5u8,
                _ => continue,
            };
            let Ok(e) = enc_scalar(lit) else { continue };
            let with = |b: u8| {
                let mut k = e.clone();
                k.push(b);
                k
            };
            match *op {
                "$gt" => (tag, lo) = (Some(t), Some(Bound::Included(with(1)))),
                "$gte" => (tag, lo) = (Some(t), Some(Bound::Included(with(0)))),
                "$lt" => (tag, hi) = (Some(t), Some(Bound::Excluded(with(0)))),
                "$lte" => (tag, hi) = (Some(t), Some(Bound::Excluded(with(1)))),
                _ => {}
            }
        }
        if let Some(t) = tag {
            let lo = lo.unwrap_or(Bound::Included(vec![t]));
            let hi = hi.unwrap_or(Bound::Excluded(vec![t + 1]));
            if best.is_none() {
                best = Some((1, Plan::Index { pos, lo, hi, kind: "range" }));
            }
        }
    }
    best.map(|(_, p)| p).unwrap_or(Plan::Full)
}

fn bound_ref(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(v) => Bound::Included(v.as_slice()),
        Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn explain_query<R: PageRead>(r: &mut R, catalog: &BTree, collection: &str, query: &Value) -> Result<String> {
    let meta = load_meta(r, catalog, collection)?;
    let q = parse_query(query)?;
    Ok(match plan_query(&meta, &q.filter) {
        Plan::Full => "full scan".to_string(),
        Plan::Index { pos, kind, .. } => format!("index scan ({kind}) on {}", meta.indexes[pos].field),
    })
}

fn type_rank(v: Option<&Value>) -> u8 {
    match v {
        None => 0,
        Some(Value::Null) => 1,
        Some(Value::Bool(_)) => 2,
        Some(Value::Number(_)) => 3,
        Some(Value::String(_)) => 4,
        Some(Value::Array(_)) => 5,
        Some(Value::Object(_)) => 6,
    }
}

fn find_docs<R: PageRead>(r: &mut R, catalog: &BTree, collection: &str, query: &Value) -> Result<Vec<Value>> {
    let meta = load_meta(r, catalog, collection)?;
    let q = parse_query(query)?;
    let primary = BTree::open(meta.primary);

    let mut docs: Vec<Value> = Vec::new();
    match plan_query(&meta, &q.filter) {
        Plan::Full => {
            for (_, v) in primary.range(r, Bound::Unbounded, Bound::Unbounded, None)? {
                let doc: Value = serde_json::from_slice(&v)?;
                if matches(&doc, &q.filter) {
                    docs.push(doc);
                }
            }
        }
        Plan::Index { pos, lo, hi, .. } => {
            let index = BTree::open(meta.indexes[pos].root);
            for (key, _) in index.range(r, bound_ref(&lo), bound_ref(&hi), None)? {
                let id = id_of_entry(&key);
                let bytes = primary.get(r, id.as_bytes())?.ok_or_else(|| {
                    EngineError::Storage(StorageError::Corrupt { page: index.root(), reason: format!("index entry points at missing document '{id}'") })
                })?;
                let doc: Value = serde_json::from_slice(&bytes)?;
                if matches(&doc, &q.filter) {
                    docs.push(doc);
                }
            }
        }
    }

    // Result order must not depend on which plan ran: order by `_id` first, then apply the
    // requested sort (stable, so ties keep `_id` order).
    docs.sort_by(|a, b| a["_id"].as_str().cmp(&b["_id"].as_str()));
    if let Some((field, asc)) = &q.sort {
        docs.sort_by(|a, b| {
            let (x, y) = (get_path(a, field), get_path(b, field));
            let ord = match (x, y) {
                (Some(x), Some(y)) if type_rank(Some(x)) == type_rank(Some(y)) => compare(x, y).unwrap_or(Ordering::Equal),
                _ => type_rank(x).cmp(&type_rank(y)),
            };
            if *asc { ord } else { ord.reverse() }
        });
    }
    let it = docs.into_iter().skip(q.skip);
    Ok(match q.limit {
        Some(n) => it.take(n).collect(),
        None => it.collect(),
    })
}

fn verify_collection<R: PageRead>(r: &mut R, catalog: &BTree, collection: &str) -> Result<Vec<String>> {
    let meta = load_meta(r, catalog, collection)?;
    let mut problems = Vec::new();
    let primary = BTree::open(meta.primary);
    if let Err(e) = primary.check(r) {
        problems.push(format!("primary tree: {e}"));
        return Ok(problems);
    }
    let docs = primary.range(r, Bound::Unbounded, Bound::Unbounded, None)?;
    for im in &meta.indexes {
        let tree = BTree::open(im.root);
        if let Err(e) = tree.check(r) {
            problems.push(format!("index '{}': {e}", im.field));
            continue;
        }
        let mut expected = BTreeSet::new();
        for (k, v) in &docs {
            let id = String::from_utf8_lossy(k).into_owned();
            let doc: Value = serde_json::from_slice(v)?;
            if let Some(key) = field_key(&doc, &im.field, &id)? {
                expected.insert(key);
            }
        }
        let actual: BTreeSet<Vec<u8>> =
            tree.range(r, Bound::Unbounded, Bound::Unbounded, None)?.into_iter().map(|(k, _)| k).collect();
        if expected != actual {
            problems.push(format!(
                "index '{}' differs from its documents ({} expected entries, {} found)",
                im.field,
                expected.len(),
                actual.len()
            ));
        }
    }
    Ok(problems)
}
