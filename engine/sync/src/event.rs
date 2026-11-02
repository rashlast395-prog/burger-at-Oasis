use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A till, phone or kitchen screen.
    Device,
    /// The remote node that relays events and is the authority for payments.
    Server,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Op {
    Upsert { doc: Value },
    Delete,
}

/// One change to one document, as recorded in every node's log.
///
/// `(device, seq)` is the identity used for replication: each device numbers its own events
/// 1, 2, 3, ... with no gaps, so any node can tell exactly which events it is missing.
/// `event_id` is a globally unique UUIDv7 for tracing and auditing.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Event {
    pub event_id: String,
    pub device: String,
    pub seq: u64,
    /// `Hlc::encode` form; sorts as a string and includes the device id as tie-break.
    pub hlc: String,
    pub role: Role,
    pub collection: String,
    pub doc_id: String,
    pub op: Op,
}

impl Event {
    /// Storage key: sorts by device, then sequence number.
    pub fn pos(&self) -> String {
        pos(&self.device, self.seq)
    }
}

pub fn pos(device: &str, seq: u64) -> String {
    format!("{device}/{seq:020}")
}
