//! Conflict rules. One rule governs each synchronised collection.
//!
//! A rule decides, from the current state of a document and one incoming event, whether the
//! event changes anything. **Rules must be pure and deterministic**: the same current state
//! and the same event always give the same decision, and the decisions must make the final
//! state independent of the order events arrive in. Every replica then converges.
//!
//! Built in:
//! * [`LastWriterWins`]: the event with the greater [`Hlc`](crate::Hlc) stamp wins; deletes
//!   leave a tombstone so an older update cannot bring a document back.
//! * [`AppendOnly`]: documents are written once and never changed or deleted (ledgers,
//!   audit trails). Re-sending the same document is harmless.
//! * [`ServerAuthoritative`]: only events from a `Server` node count (payment results);
//!   anything else is rejected into the dead-letter log. Otherwise last-writer-wins.
//! * [`Versioned`]: documents carry an integer `version`; the highest version wins, ties
//!   broken by stamp (catalogue items, prices).

use serde_json::Value;

use crate::event::{Event, Op, Role};

/// What the node currently holds for one document.
#[derive(Debug, Clone, Default)]
pub struct Current {
    pub doc: Option<Value>,
    /// Stamp of the event that wrote `doc`.
    pub doc_hlc: Option<String>,
    /// Stamp of the delete event, if the document was deleted.
    pub tombstone_hlc: Option<String>,
}

impl Current {
    fn latest_hlc(&self) -> Option<&String> {
        match (&self.doc_hlc, &self.tombstone_hlc) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.as_ref().or(b.as_ref()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Change the document as the event says.
    Apply,
    /// Valid, but superseded or redundant: nothing changes and nothing is reported.
    Skip(String),
    /// Not acceptable (wrong sender, forbidden operation): kept in the dead-letter log.
    Reject(String),
}

pub trait Rule: Send + Sync {
    /// Stable name. It is stored in the database; opening with a different rule is refused,
    /// because replicas using different rules would diverge.
    fn name(&self) -> &'static str;

    fn decide(&self, current: &Current, event: &Event) -> Decision;

    /// Extra check applied only to events created on this node, before they are logged.
    /// Unlike `decide`, it may depend on what this node currently holds (`current`), so it
    /// can enforce transitions such as "an order may only move forward". It must not be
    /// relied on for convergence: other nodes never run it on this node's events.
    fn local_check(&self, _role: Role, _current: &Current, _event: &Event) -> Result<(), String> {
        Ok(())
    }
}

fn user_doc(ev: &Event) -> Option<&Value> {
    match &ev.op {
        Op::Upsert { doc } => Some(doc),
        Op::Delete => None,
    }
}

fn lww(current: &Current, ev: &Event) -> Decision {
    match current.latest_hlc() {
        Some(latest) if ev.hlc <= *latest => Decision::Skip("a newer write already exists".into()),
        _ => Decision::Apply,
    }
}

/// Document content without the bookkeeping fields the node adds.
pub fn strip_meta(doc: &Value) -> Value {
    let mut d = doc.clone();
    if let Value::Object(m) = &mut d {
        m.remove("_hlc");
        m.remove("_origin");
    }
    d
}

pub struct LastWriterWins;

impl Rule for LastWriterWins {
    fn name(&self) -> &'static str {
        "last-writer-wins"
    }
    fn decide(&self, current: &Current, ev: &Event) -> Decision {
        lww(current, ev)
    }
}

pub struct AppendOnly;

impl Rule for AppendOnly {
    fn name(&self) -> &'static str {
        "append-only"
    }
    fn decide(&self, current: &Current, ev: &Event) -> Decision {
        let Some(doc) = user_doc(ev) else {
            return Decision::Reject("append-only collections do not allow deletes".into());
        };
        match (&current.doc, &current.doc_hlc) {
            (None, _) => Decision::Apply,
            (Some(existing), Some(existing_hlc)) => {
                if strip_meta(existing) == *doc {
                    Decision::Skip("already recorded".into())
                } else if ev.hlc < *existing_hlc {
                    // Two different documents claimed one id. Keep the earlier stamp on
                    // every replica so they agree, whatever order the events arrived in.
                    Decision::Apply
                } else {
                    Decision::Skip("an earlier document already holds this id".into())
                }
            }
            (Some(_), None) => Decision::Skip("already recorded".into()),
        }
    }
}

pub struct ServerAuthoritative;

impl Rule for ServerAuthoritative {
    fn name(&self) -> &'static str {
        "server-authoritative"
    }
    fn decide(&self, current: &Current, ev: &Event) -> Decision {
        if ev.role != Role::Server {
            return Decision::Reject("only the server may write this collection".into());
        }
        lww(current, ev)
    }
    fn local_check(&self, role: Role, _current: &Current, _ev: &Event) -> Result<(), String> {
        if role == Role::Server {
            Ok(())
        } else {
            Err("only the server may write this collection".into())
        }
    }
}

pub struct Versioned;

fn version_of(doc: &Value) -> Option<u64> {
    doc.get("version").and_then(Value::as_u64)
}

impl Rule for Versioned {
    fn name(&self) -> &'static str {
        "versioned"
    }
    fn decide(&self, current: &Current, ev: &Event) -> Decision {
        let Some(doc) = user_doc(ev) else {
            return Decision::Reject("versioned collections do not allow deletes; bump the version instead".into());
        };
        let Some(incoming) = version_of(doc).filter(|v| *v >= 1) else {
            return Decision::Reject("document needs an integer \"version\" of at least 1".into());
        };
        let (Some(existing), Some(existing_hlc)) = (&current.doc, &current.doc_hlc) else {
            return Decision::Apply;
        };
        match version_of(existing).unwrap_or(0).cmp(&incoming) {
            std::cmp::Ordering::Less => Decision::Apply,
            std::cmp::Ordering::Greater => Decision::Skip("a higher version already exists".into()),
            std::cmp::Ordering::Equal if strip_meta(existing) == *doc => Decision::Skip("same version already recorded".into()),
            // Same version, different content: the later stamp wins on every replica.
            std::cmp::Ordering::Equal if ev.hlc > *existing_hlc => Decision::Apply,
            std::cmp::Ordering::Equal => Decision::Skip("same version, an earlier-stamped edit loses".into()),
        }
    }
}
