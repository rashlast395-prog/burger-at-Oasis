//! Restaurant rules for Burger Oasis, built on `bo-sync`.
//!
//! ## Orders: a state machine that still converges offline
//! An order moves `placed → accepted → preparing → ready → (out_for_delivery →) completed`,
//! and can be `cancelled` before it is completed.
//!
//! Two layers keep this safe when devices work offline:
//! * **On the device that makes a change** ([`OrderRule::local_check`]): the transition must
//!   be allowed from the status that device currently sees, and nothing but the status may
//!   change (items, prices and totals are fixed once placed).
//! * **Everywhere else** ([`OrderRule::decide`]): replicas cannot re-check transitions, because
//!   they may hold different histories and would then disagree. Instead every replica keeps
//!   the event with the greatest `(status rank, clock stamp)`. That is a maximum over a total
//!   order, so every replica ends with the same order whatever the arrival order.
//!
//! The ranks decide real conflicts: `cancelled` outranks every in-progress status (a stale
//! "ready" from the kitchen cannot revive a cancelled order), and `completed` outranks
//! `cancelled` (once handed over and closed, a late cancel from another till loses).
//!
//! ## Inventory: an append-only ledger
//! Stock is never stored as a number that devices overwrite. Every change is a ledger entry
//! (`sale`, `sale_reversal`, `restock`, `waste`, `adjustment`) and the stock level is the sum.
//! Entries created for an order have **deterministic ids** (`sale:<order>:<stock sku>`), so two
//! devices recording the same sale produce the same entry, not a double deduction.
//!
//! * Accepting an order deducts its ingredients (from `recipes`, or 1:1 for an item without a
//!   recipe, such as a bottled drink) in the same local transaction as the status change.
//! * Cancelling an order that was deducted adds reversals.
//! * Offline races leave temporary mismatches: one till accepts while another cancels (a
//!   cancelled order with a deduction), or a till cancels and reverses while another
//!   completes (a completed order with a stray reversal). [`reconcile`] compares each order's
//!   net stock effect with what its converged status requires and writes the difference; the
//!   server runs it every 30 seconds.
//!
//! ## Business choices made here (change them if the restaurant works differently)
//! * Money is integer minor units (pesewas for GHS); no floating point anywhere.
//! * Stock is deducted at `accepted`, not at `completed`.
//! * Stock may go negative: an offline till cannot know what other tills sold, so overselling
//!   is reported ([`oversold`]) rather than prevented.
//! * Order contents cannot be edited after placing; cancel and re-place instead.
//!
//! ## Known limits
//! * Remote replicas trust the transition the originating device checked; a buggy or
//!   compromised (but enrolled and signing) device could jump an order straight to `completed`.
//! * If a recipe changes while devices are offline, two devices can compute different sale
//!   entries for one order; one wins per stock item and a stocktake adjustment fixes the rest.
//! * `reconcile` scans every order; fine for now, needs an index or watermark at scale.

use std::collections::BTreeMap;
use std::fmt;

use bo_sync::{new_uuid_v7, strip_meta, AppendOnly, Current, Decision, Event, Op, Role, Rule, SyncError, SyncNode, Write};
use serde_json::{json, Map, Value};

pub const ORDERS: &str = "orders";
pub const LEDGER: &str = "inventory_ledger";
pub const RECIPES: &str = "recipes";
pub const PAYMENTS: &str = "payments";

pub mod payments;
pub use payments::{payment_anomalies, payment_summary, record_cash, record_online, record_refund, Anomaly, PaymentLedger, PaymentSummary};

/// Order statuses in rank order (index + 1 is the rank).
pub const STATUSES: [&str; 7] = ["placed", "accepted", "preparing", "ready", "out_for_delivery", "cancelled", "completed"];

const MAX_ITEMS: usize = 200;
const MAX_QTY: i64 = 1000;
pub(crate) const MAX_MONEY: i64 = 100_000_000_00; // 100 million in major units
const MAX_STOCK_DELTA: i64 = 1_000_000;

pub fn rank(status: &str) -> Option<u8> {
    STATUSES.iter().position(|s| *s == status).map(|p| p as u8 + 1)
}

/// Transitions a device may make from the status it currently sees.
pub fn allowed_next(from: &str) -> &'static [&'static str] {
    match from {
        "placed" => &["accepted", "cancelled"],
        "accepted" => &["preparing", "cancelled"],
        "preparing" => &["ready", "cancelled"],
        "ready" => &["out_for_delivery", "completed", "cancelled"],
        "out_for_delivery" => &["completed", "cancelled"],
        _ => &[], // cancelled and completed are final
    }
}

#[derive(Debug)]
pub enum DomainError {
    Invalid(String),
    NotFound(String),
    Sync(SyncError),
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DomainError::Invalid(m) => write!(f, "{m}"),
            DomainError::NotFound(m) => write!(f, "not found: {m}"),
            DomainError::Sync(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DomainError {}

impl From<SyncError> for DomainError {
    fn from(e: SyncError) -> Self {
        DomainError::Sync(e)
    }
}

pub type Result<T> = std::result::Result<T, DomainError>;

// ---------------------------------------------------------------------- validation

pub(crate) fn int(v: &Value, field: &str, min: i64, max: i64) -> std::result::Result<i64, String> {
    match v.get(field) {
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => {
            let x = n.as_i64().ok_or_else(|| format!("{field} is too large"))?;
            if x < min || x > max {
                Err(format!("{field} must be between {min} and {max}"))
            } else {
                Ok(x)
            }
        }
        Some(_) => Err(format!("{field} must be a whole number (money is in minor units, e.g. pesewas)")),
        None => Err(format!("{field} is required")),
    }
}

pub(crate) fn text<'a>(v: &'a Value, field: &str, max: usize) -> std::result::Result<&'a str, String> {
    match v.get(field) {
        Some(Value::String(s)) if !s.trim().is_empty() && s.len() <= max => Ok(s),
        Some(Value::String(_)) => Err(format!("{field} must be 1 to {max} characters")),
        _ => Err(format!("{field} is required")),
    }
}

/// Check an order document on its own (no history needed): shape, money and totals.
pub fn validate_order(doc: &Value) -> std::result::Result<(), String> {
    let status = text(doc, "status", 32)?;
    rank(status).ok_or_else(|| format!("unknown status '{status}'"))?;
    let currency = text(doc, "currency", 3)?;
    if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err("currency must be a 3-letter code such as GHS".into());
    }
    let items = doc.get("items").and_then(Value::as_array).ok_or("items must be a list")?;
    if items.is_empty() || items.len() > MAX_ITEMS {
        return Err(format!("an order needs 1 to {MAX_ITEMS} items"));
    }
    let mut subtotal: i64 = 0;
    for (i, it) in items.iter().enumerate() {
        let at = |e: String| format!("item {}: {e}", i + 1);
        text(it, "sku", 64).map_err(at)?;
        text(it, "name", 200).map_err(at)?;
        let qty = int(it, "qty", 1, MAX_QTY).map_err(at)?;
        let price = int(it, "unit_price", 0, MAX_MONEY).map_err(at)?;
        subtotal += qty * price;
    }
    let delivery = int(doc, "delivery_fee", 0, MAX_MONEY)?;
    let discount = int(doc, "discount", 0, MAX_MONEY)?;
    if int(doc, "subtotal", 0, i64::MAX)? != subtotal {
        return Err(format!("subtotal must equal the sum of the items ({subtotal})"));
    }
    let total = subtotal + delivery - discount;
    if total < 0 {
        return Err("discount is larger than the order".into());
    }
    if int(doc, "total", 0, i64::MAX)? != total {
        return Err(format!("total must equal subtotal + delivery_fee - discount ({total})"));
    }
    if let Some(n) = doc.get("notes") {
        if !n.as_str().map_or(false, |s| s.len() <= 500) {
            return Err("notes must be text of at most 500 characters".into());
        }
    }
    Ok(())
}

/// Check a ledger entry on its own.
pub fn validate_entry(doc: &Value) -> std::result::Result<(), String> {
    text(doc, "sku", 64)?;
    let delta = int(doc, "delta", -MAX_STOCK_DELTA, MAX_STOCK_DELTA)?;
    if delta == 0 {
        return Err("delta must not be zero".into());
    }
    let reason = text(doc, "reason", 32)?;
    let sign_ok = match reason {
        "sale" | "waste" => delta < 0,
        "sale_reversal" | "restock" => delta > 0,
        "adjustment" => true,
        _ => return Err(format!("unknown reason '{reason}'")),
    };
    if !sign_ok {
        return Err(format!("a {reason} entry cannot have delta {delta}"));
    }
    if matches!(reason, "sale" | "sale_reversal") {
        text(doc, "order_id", 100)?;
    }
    Ok(())
}

pub(crate) fn upsert_doc(ev: &Event) -> Option<&Value> {
    match &ev.op {
        Op::Upsert { doc } => Some(doc),
        Op::Delete => None,
    }
}

fn status_of(doc: &Value) -> &str {
    doc.get("status").and_then(Value::as_str).unwrap_or("")
}

/// Everything except the status (and the node's bookkeeping fields).
fn contents(doc: &Value) -> Value {
    let mut d = strip_meta(doc);
    if let Value::Object(m) = &mut d {
        m.remove("status");
    }
    d
}

// ---------------------------------------------------------------------- rules

pub struct OrderRule;

impl Rule for OrderRule {
    fn name(&self) -> &'static str {
        "order"
    }

    fn decide(&self, cur: &Current, ev: &Event) -> Decision {
        let Some(doc) = upsert_doc(ev) else {
            return Decision::Reject("orders are never deleted; cancel them instead".into());
        };
        if let Err(e) = validate_order(doc) {
            return Decision::Reject(e);
        }
        let (Some(existing), Some(existing_hlc)) = (&cur.doc, &cur.doc_hlc) else {
            return Decision::Apply;
        };
        let incoming = (rank(status_of(doc)).unwrap_or(0), &ev.hlc);
        let held = (rank(status_of(existing)).unwrap_or(0), existing_hlc);
        if incoming > held {
            Decision::Apply
        } else {
            Decision::Skip("a later or further-along status is already held".into())
        }
    }

    fn local_check(&self, _role: Role, cur: &Current, ev: &Event) -> std::result::Result<(), String> {
        let Some(doc) = upsert_doc(ev) else {
            return Err("orders are never deleted; cancel them instead".into());
        };
        let to = status_of(doc);
        match &cur.doc {
            None if to == "placed" => Ok(()),
            None => Err("a new order must start as 'placed'".into()),
            Some(existing) => {
                let from = status_of(existing);
                if !allowed_next(from).contains(&to) {
                    return Err(format!("an order cannot go from '{from}' to '{to}'"));
                }
                if contents(existing) != contents(doc) {
                    return Err("only the status of a placed order can change; cancel and place a new order instead".into());
                }
                Ok(())
            }
        }
    }
}

pub struct InventoryLedger;

impl Rule for InventoryLedger {
    fn name(&self) -> &'static str {
        "inventory-ledger"
    }

    fn decide(&self, cur: &Current, ev: &Event) -> Decision {
        if let Some(doc) = upsert_doc(ev) {
            if let Err(e) = validate_entry(doc) {
                return Decision::Reject(e);
            }
        }
        AppendOnly.decide(cur, ev) // rejects deletes, ignores identical re-sends
    }
}

/// The three collections this crate manages and their rules, for `SyncNode::create/open`.
pub fn rules() -> Vec<(String, Box<dyn Rule>)> {
    vec![
        (ORDERS.into(), Box::new(OrderRule)),
        (LEDGER.into(), Box::new(InventoryLedger)),
        (RECIPES.into(), Box::new(bo_sync::Versioned)),
        (PAYMENTS.into(), Box::new(PaymentLedger)),
    ]
}

// ---------------------------------------------------------------------- operations

/// Place a new order. `draft` needs `items` (each `sku`, `name`, `qty`, `unit_price` in minor
/// units) and `currency`; it may have `delivery_fee`, `discount`, `notes`, `customer` and
/// `_id`. Subtotal and total are computed here. Returns the stored order.
pub fn place_order(node: &SyncNode, draft: Value) -> Result<Value> {
    let Value::Object(mut m) = draft else {
        return Err(DomainError::Invalid("an order must be a JSON object".into()));
    };
    let items = m.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut subtotal: i64 = 0;
    for it in &items {
        let q = it.get("qty").and_then(Value::as_i64).unwrap_or(0);
        let p = it.get("unit_price").and_then(Value::as_i64).unwrap_or(0);
        subtotal = subtotal.saturating_add(q.saturating_mul(p));
    }
    let delivery = m.get("delivery_fee").and_then(Value::as_i64).unwrap_or(0);
    let discount = m.get("discount").and_then(Value::as_i64).unwrap_or(0);
    m.entry("_id").or_insert_with(|| json!(new_uuid_v7(node.now_ms())));
    m.entry("delivery_fee").or_insert(json!(0));
    m.entry("discount").or_insert(json!(0));
    m.insert("status".into(), json!("placed"));
    m.insert("subtotal".into(), json!(subtotal));
    m.insert("total".into(), json!(subtotal + delivery - discount));
    m.insert("created_at".into(), json!(node.now_ms()));
    let doc = Value::Object(m);
    validate_order(&doc).map_err(DomainError::Invalid)?;
    let id = doc["_id"].as_str().unwrap_or_default().to_string();
    node.put(ORDERS, doc)?;
    node.get(ORDERS, &id)?.ok_or(DomainError::NotFound(id))
}

/// Stock consumed by one unit of `sku`: its recipe's components, or the item itself.
fn components(node: &SyncNode, sku: &str) -> Result<Vec<(String, i64)>> {
    match node.get(RECIPES, sku)? {
        Some(r) => Ok(r["components"]
            .as_array()
            .map(|cs| cs.iter().filter_map(|c| Some((c["stock_sku"].as_str()?.to_string(), c["qty"].as_i64()?))).filter(|(_, q)| *q > 0).collect())
            .unwrap_or_default()),
        None => Ok(vec![(sku.to_string(), 1)]),
    }
}

/// Stock to deduct for an order, summed per stock item.
fn usage(node: &SyncNode, order: &Value) -> Result<BTreeMap<String, i64>> {
    let mut total = BTreeMap::new();
    for it in order["items"].as_array().into_iter().flatten() {
        let (Some(sku), Some(qty)) = (it["sku"].as_str(), it["qty"].as_i64()) else { continue };
        for (stock, per) in components(node, sku)? {
            *total.entry(stock).or_insert(0) += per * qty;
        }
    }
    Ok(total)
}

/// The ledger writes that bring an order's stock effect in line with its status.
///
/// Works on the **net** effect per stock item, so it repairs any mix of races (a cancel that
/// reversed nothing, a reversal written by a till whose cancel later lost to `completed`,
/// deductions recorded twice under different ids...):
/// * an active order (accepted or later, not cancelled) should net to its original sale
///   entry per item, or to its recipe usage if it has none yet;
/// * any other order (placed or cancelled) should net to zero.
///
/// The first entry for an item is `sale:<order>:<sku>`; corrections are
/// `adj:<order>:<sku>:<n>` where `n` counts the entries already seen. Two nodes with the
/// same view therefore write the same ids, and a node with a stale view that over-corrects
/// is itself corrected by the next reconcile.
fn stock_writes(node: &SyncNode, order: &Value) -> Result<Vec<Write>> {
    let id = order["_id"].as_str().unwrap_or_default();
    let status = status_of(order);
    let active = status != "cancelled" && rank(status).unwrap_or(0) >= rank("accepted").unwrap_or(0);

    let mut net: BTreeMap<String, i64> = BTreeMap::new();
    let mut original: BTreeMap<String, i64> = BTreeMap::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for e in node.find(LEDGER, &json!({"filter": {"order_id": id}}))? {
        let (Some(sku), Some(d)) = (e["sku"].as_str(), e["delta"].as_i64()) else { continue };
        *net.entry(sku.to_string()).or_insert(0) += d;
        *seen.entry(sku.to_string()).or_insert(0) += 1;
        if e["_id"].as_str().is_some_and(|x| x.starts_with("sale:")) {
            original.insert(sku.to_string(), d);
        }
    }
    let target: BTreeMap<String, i64> = if !active {
        BTreeMap::new()
    } else if original.is_empty() {
        usage(node, order)?.into_iter().filter(|(_, q)| *q > 0).map(|(s, q)| (s, -q)).collect()
    } else {
        original
    };

    let now = node.now_ms();
    let mut writes = Vec::new();
    for sku in target.keys().chain(net.keys()).collect::<std::collections::BTreeSet<_>>() {
        let diff = target.get(sku).copied().unwrap_or(0) - net.get(sku).copied().unwrap_or(0);
        if diff == 0 {
            continue;
        }
        let n = seen.get(sku).copied().unwrap_or(0);
        let entry_id = if n == 0 { format!("sale:{id}:{sku}") } else { format!("adj:{id}:{sku}:{n}") };
        let reason = if diff < 0 { "sale" } else { "sale_reversal" };
        writes.push(Write::Put {
            collection: LEDGER.into(),
            doc: json!({"_id": entry_id, "sku": sku, "delta": diff, "reason": reason, "order_id": id, "at": now}),
        });
    }
    Ok(writes)
}

/// Move an order to `status`, adjusting stock in the same local transaction. Returns the
/// stored order.
pub fn set_status(node: &SyncNode, order_id: &str, status: &str) -> Result<Value> {
    let current = node.get(ORDERS, order_id)?.ok_or_else(|| DomainError::NotFound(format!("order {order_id}")))?;
    let mut next = strip_meta(&current);
    next["status"] = json!(status);
    let mut writes = vec![Write::Put { collection: ORDERS.into(), doc: next.clone() }];
    writes.extend(stock_writes(node, &next)?);
    // The order rule re-checks the transition inside the transaction, so a status that
    // changed since `get` above is refused rather than overwritten.
    node.commit(writes)?;
    node.get(ORDERS, order_id)?.ok_or_else(|| DomainError::NotFound(format!("order {order_id}")))
}

/// Make every order's stock effect match its (converged) status: add missing sale entries
/// for accepted orders and missing reversals for cancelled ones. Idempotent: entries have
/// deterministic ids, so running it on several nodes cannot double count. Returns the
/// number of entries written.
pub fn reconcile(node: &SyncNode) -> Result<usize> {
    let mut written = 0;
    for order in node.find(ORDERS, &json!({}))? {
        let writes = stock_writes(node, &order)?;
        if !writes.is_empty() {
            written += node.commit(writes)?.iter().filter(|e| e.is_some()).count();
        }
    }
    Ok(written)
}

/// Record stock arriving, being thrown away, or a stocktake correction.
pub fn record_stock(node: &SyncNode, sku: &str, delta: i64, reason: &str, note: Option<&str>) -> Result<()> {
    if matches!(reason, "sale" | "sale_reversal") {
        return Err(DomainError::Invalid("sales are recorded through orders".into()));
    }
    let mut doc = Map::new();
    doc.insert("_id".into(), json!(format!("{reason}:{}", new_uuid_v7(node.now_ms()))));
    doc.insert("sku".into(), json!(sku));
    doc.insert("delta".into(), json!(delta));
    doc.insert("reason".into(), json!(reason));
    doc.insert("at".into(), json!(node.now_ms()));
    if let Some(n) = note {
        doc.insert("note".into(), json!(n));
    }
    let doc = Value::Object(doc);
    validate_entry(&doc).map_err(DomainError::Invalid)?;
    node.put(LEDGER, doc)?;
    Ok(())
}

/// Current stock of every item that has ledger entries.
pub fn stock_levels(node: &SyncNode) -> Result<BTreeMap<String, i64>> {
    let mut levels = BTreeMap::new();
    for e in node.find(LEDGER, &json!({}))? {
        if let (Some(sku), Some(d)) = (e["sku"].as_str(), e["delta"].as_i64()) {
            *levels.entry(sku.to_string()).or_insert(0) += d;
        }
    }
    Ok(levels)
}

/// Items whose stock is below zero (sold while offline beyond what was on hand).
pub fn oversold(node: &SyncNode) -> Result<BTreeMap<String, i64>> {
    Ok(stock_levels(node)?.into_iter().filter(|(_, v)| *v < 0).collect())
}
