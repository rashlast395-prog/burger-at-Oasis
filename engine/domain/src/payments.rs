//! Payments: an append-only money ledger.
//!
//! Every movement of money is one immutable entry: `kind` is `payment` (money in) or
//! `refund` (money out), with an integer `amount` in minor units. What an order has been
//! paid is the sum, so nothing is ever overwritten and two devices cannot lose each other's
//! records.
//!
//! ## Who may record what
//! * **Cash** (`method: "cash"`): any enrolled device, offline included. A till takes cash
//!   whether or not the internet is up.
//! * **Card and mobile money**: only the **server** (`Role::Server`). The rule checks the
//!   event's role, which signatures bind to the enrolled device, so a till cannot claim
//!   that a card payment succeeded. These entries are keyed `<provider>:<reference>`, so a
//!   provider notification delivered twice records once.
//!
//! [`record_online`] takes data that has **already been verified with the provider**. The
//! Paystack integration (receive the webhook, check its signature, then call Paystack's
//! verify endpoint with the secret key from the server's environment) is not built yet,
//! because it needs the restaurant's Paystack account. Never call `record_online` on the
//! strength of a browser redirect or a client's say-so.
//!
//! ## Never automatic
//! Nothing here moves money on its own. Situations that need a person - an order completed
//! but not fully paid, a cancelled order still holding money, two offline tills refunding
//! the same order - are reported by [`payment_anomalies`], not "fixed".

use std::collections::BTreeMap;

use bo_sync::{new_uuid_v7, AppendOnly, Current, Decision, Event, Role, Rule, SyncNode};
use serde_json::{json, Value};

use crate::{int, text, upsert_doc, DomainError, Result, MAX_MONEY, ORDERS, PAYMENTS};

pub const METHODS: [&str; 3] = ["cash", "card", "mobile_money"];

/// Check a payment entry on its own (shape and money; who sent it is checked by the rule).
pub fn validate_payment(doc: &Value) -> std::result::Result<(), String> {
    text(doc, "order_id", 100)?;
    let kind = text(doc, "kind", 16)?;
    if !matches!(kind, "payment" | "refund") {
        return Err(format!("kind must be 'payment' or 'refund', not '{kind}'"));
    }
    let method = text(doc, "method", 32)?;
    if !METHODS.contains(&method) {
        return Err(format!("unknown payment method '{method}'"));
    }
    int(doc, "amount", 1, MAX_MONEY)?;
    let currency = text(doc, "currency", 3)?;
    if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err("currency must be a 3-letter code such as GHS".into());
    }
    if method != "cash" {
        let provider = text(doc, "provider", 32)?;
        let reference = text(doc, "reference", 100)?;
        let want = format!("{provider}:{reference}:{kind}");
        if doc.get("_id").and_then(Value::as_str) != Some(want.as_str()) {
            return Err(format!("a {method} entry must have the id '{want}'"));
        }
    }
    Ok(())
}

pub struct PaymentLedger;

impl Rule for PaymentLedger {
    fn name(&self) -> &'static str {
        "payment-ledger"
    }

    fn decide(&self, cur: &Current, ev: &Event) -> Decision {
        if let Some(doc) = upsert_doc(ev) {
            if let Err(e) = validate_payment(doc) {
                return Decision::Reject(e);
            }
            if doc["method"] != "cash" && ev.role != Role::Server {
                return Decision::Reject("only the server may record card or mobile money payments".into());
            }
        }
        AppendOnly.decide(cur, ev) // no deletes; identical re-sends change nothing
    }
}

/// Money position of one order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentSummary {
    pub total: i64,
    pub paid: i64,
    pub refunded: i64,
    /// `paid - refunded`
    pub net: i64,
    /// `unpaid`, `partially_paid`, `paid`, `overpaid` or `refunded`
    pub status: &'static str,
}

fn order(node: &SyncNode, order_id: &str) -> Result<Value> {
    node.get(ORDERS, order_id)?.ok_or_else(|| DomainError::NotFound(format!("order {order_id}")))
}

fn entries(node: &SyncNode, order_id: &str) -> Result<Vec<Value>> {
    Ok(node.find(PAYMENTS, &json!({"filter": {"order_id": order_id}}))?)
}

fn summarise(total: i64, entries: &[Value]) -> PaymentSummary {
    let sum = |kind: &str| entries.iter().filter(|e| e["kind"] == kind).filter_map(|e| e["amount"].as_i64()).sum::<i64>();
    let (paid, refunded) = (sum("payment"), sum("refund"));
    let net = paid - refunded;
    let status = if net == 0 && refunded > 0 {
        "refunded"
    } else if net <= 0 {
        "unpaid"
    } else if net < total {
        "partially_paid"
    } else if net == total {
        "paid"
    } else {
        "overpaid"
    };
    PaymentSummary { total, paid, refunded, net, status }
}

pub fn payment_summary(node: &SyncNode, order_id: &str) -> Result<PaymentSummary> {
    let o = order(node, order_id)?;
    Ok(summarise(o["total"].as_i64().unwrap_or(0), &entries(node, order_id)?))
}

fn checked_order(node: &SyncNode, order_id: &str, currency: &str) -> Result<Value> {
    let o = order(node, order_id)?;
    if o["currency"].as_str() != Some(currency) {
        return Err(DomainError::Invalid(format!("order {order_id} is in {}, not {currency}", o["currency"].as_str().unwrap_or("?"))));
    }
    Ok(o)
}

/// Record cash taken at a till. Any amount is accepted (customers overpay and get change
/// back as a separate refund, or pay in parts); the summary shows where the order stands.
pub fn record_cash(node: &SyncNode, order_id: &str, amount: i64, note: Option<&str>) -> Result<PaymentSummary> {
    let o = order(node, order_id)?;
    let currency = o["currency"].as_str().unwrap_or_default().to_string();
    if o["status"] == "cancelled" {
        return Err(DomainError::Invalid("this order is cancelled; do not take payment for it".into()));
    }
    let mut doc = json!({"_id": format!("cash:{}", new_uuid_v7(node.now_ms())), "order_id": order_id, "kind": "payment",
        "method": "cash", "amount": amount, "currency": currency, "at": node.now_ms()});
    if let Some(n) = note {
        doc["note"] = json!(n);
    }
    validate_payment(&doc).map_err(DomainError::Invalid)?;
    node.put(PAYMENTS, doc)?;
    payment_summary(node, order_id)
}

/// Give money back. Refused if it would refund more than this device knows was paid; two
/// offline devices can still both refund, which [`payment_anomalies`] reports. Card and
/// mobile money refunds must go through the provider and are recorded by the server with
/// [`record_online`].
pub fn record_refund(node: &SyncNode, order_id: &str, amount: i64, note: Option<&str>) -> Result<PaymentSummary> {
    let o = order(node, order_id)?;
    let s = payment_summary(node, order_id)?;
    if amount > s.net {
        return Err(DomainError::Invalid(format!("cannot refund {amount}: only {} has been paid on this order", s.net)));
    }
    let mut doc = json!({"_id": format!("cash-refund:{}", new_uuid_v7(node.now_ms())), "order_id": order_id, "kind": "refund",
        "method": "cash", "amount": amount, "currency": o["currency"], "at": node.now_ms()});
    if let Some(n) = note {
        doc["note"] = json!(n);
    }
    validate_payment(&doc).map_err(DomainError::Invalid)?;
    node.put(PAYMENTS, doc)?;
    payment_summary(node, order_id)
}

/// Server only: record a card or mobile money payment (or refund) **that has already been
/// verified with the provider**. Idempotent per `(provider, reference, kind)`.
pub fn record_online(node: &SyncNode, order_id: &str, kind: &str, method: &str, provider: &str, reference: &str, amount: i64, currency: &str) -> Result<PaymentSummary> {
    if node.role() != Role::Server {
        return Err(DomainError::Invalid("only the server records card or mobile money payments".into()));
    }
    if method == "cash" {
        return Err(DomainError::Invalid("use record_cash for cash".into()));
    }
    checked_order(node, order_id, currency)?;
    let doc = json!({"_id": format!("{provider}:{reference}:{kind}"), "order_id": order_id, "kind": kind, "method": method,
        "provider": provider, "reference": reference, "amount": amount, "currency": currency, "at": node.now_ms()});
    validate_payment(&doc).map_err(DomainError::Invalid)?;
    node.put(PAYMENTS, doc)?;
    payment_summary(node, order_id)
}

/// Something about an order's money that a person should look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anomaly {
    pub order_id: String,
    pub problem: String,
}

/// Orders whose money does not match their state. Pure report: changes nothing.
pub fn payment_anomalies(node: &SyncNode) -> Result<Vec<Anomaly>> {
    let mut by_order: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for e in node.find(PAYMENTS, &json!({}))? {
        if let Some(id) = e["order_id"].as_str() {
            by_order.entry(id.to_string()).or_default().push(e);
        }
    }
    let mut out = Vec::new();
    let mut flag = |id: &str, p: String| out.push(Anomaly { order_id: id.to_string(), problem: p });
    for o in node.find(ORDERS, &json!({}))? {
        let id = o["_id"].as_str().unwrap_or_default();
        let es = by_order.remove(id).unwrap_or_default();
        let s = summarise(o["total"].as_i64().unwrap_or(0), &es);
        let currency = o["currency"].as_str().unwrap_or_default();
        if es.iter().any(|e| e["currency"].as_str() != Some(currency)) {
            flag(id, format!("payments in a currency other than the order's {currency}"));
        }
        if s.net < 0 {
            flag(id, format!("refunded {} more than was paid", -s.net));
        }
        match o["status"].as_str().unwrap_or_default() {
            "completed" if s.net < s.total => flag(id, format!("completed but only {} of {} paid", s.net.max(0), s.total)),
            "cancelled" if s.net > 0 => flag(id, format!("cancelled but still holding {} - refund it", s.net)),
            _ => {}
        }
        if s.net > s.total {
            flag(id, format!("overpaid by {}", s.net - s.total));
        }
    }
    for id in by_order.keys() {
        flag(id, "payments for an order this node does not know".into());
    }
    Ok(out)
}
