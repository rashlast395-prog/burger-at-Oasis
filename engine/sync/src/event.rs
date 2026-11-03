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
    /// Ed25519 signature (hex) by `device` over [`Event::signing_bytes`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl Event {
    /// The exact bytes that are signed: every field, length-prefixed so no two different
    /// events share an encoding. (Document keys serialise in sorted order, so the bytes are
    /// the same on every node.)
    pub fn signing_bytes(&self) -> Vec<u8> {
        fn part(b: &mut Vec<u8>, s: &[u8]) {
            b.extend_from_slice(&(s.len() as u32).to_be_bytes());
            b.extend_from_slice(s);
        }
        let mut b = b"bo-event-v1".to_vec();
        for s in [&self.event_id, &self.device, &self.hlc, &self.collection, &self.doc_id] {
            part(&mut b, s.as_bytes());
        }
        b.extend_from_slice(&self.seq.to_be_bytes());
        b.push(match self.role {
            Role::Device => 1,
            Role::Server => 2,
        });
        match &self.op {
            Op::Upsert { doc } => {
                b.push(1);
                part(&mut b, &serde_json::to_vec(doc).unwrap_or_default());
            }
            Op::Delete => b.push(2),
        }
        b
    }

    /// Storage key: sorts by device, then sequence number.
    pub fn pos(&self) -> String {
        pos(&self.device, self.seq)
    }
}

pub fn pos(device: &str, seq: u64) -> String {
    format!("{device}/{seq:020}")
}
