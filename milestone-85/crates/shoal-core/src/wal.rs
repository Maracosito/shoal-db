//! Write-ahead log.
//!
//! Each write batch is one immutable object: `namespaces/{ns}/wal/{seq:020}.wal`.
//! Object writes are atomic on all supported backends, so a WAL object either
//! exists completely or not at all. Sequence numbers are allocated by the
//! single writer per namespace and claimed with a conditional create, which
//! both detects writer races and makes retried writes safe.
//!
//! Patch and delete-by-filter operations are resolved to concrete
//! upserts/deletes *before* the WAL write, so replay is fully deterministic
//! and requires no segment reads.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::framing;
use crate::types::{DocId, Document};
use bytes::Bytes;

pub const WAL_MAGIC: &[u8; 8] = b"SHOALWL1";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WalRecord {
    Upsert { docs: Vec<Document> },
    Delete { ids: Vec<DocId> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalEntry {
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub ts_ms: i64,
    pub record: WalRecord,
}

pub fn encode_entry(entry: &WalEntry) -> Result<Bytes> {
    let body = serde_json::to_vec(entry)?;
    Ok(framing::encode_framed(WAL_MAGIC, &body))
}

pub fn decode_entry(data: &[u8]) -> Result<WalEntry> {
    let body = framing::decode_framed(WAL_MAGIC, data)?;
    Ok(serde_json::from_slice(body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::now_ms;
    use std::collections::BTreeMap;

    #[test]
    fn roundtrip() {
        let entry = WalEntry {
            seq: 7,
            request_id: Some("req-1".into()),
            ts_ms: now_ms(),
            record: WalRecord::Upsert {
                docs: vec![Document {
                    id: "a".into(),
                    vector: Some(vec![1.0, 2.0]),
                    sparse_vector: None,
                    attributes: BTreeMap::new(),
                }],
            },
        };
        let bytes = encode_entry(&entry).unwrap();
        let back = decode_entry(&bytes).unwrap();
        assert_eq!(back.seq, 7);
        assert_eq!(back.request_id.as_deref(), Some("req-1"));
        match back.record {
            WalRecord::Upsert { docs } => assert_eq!(docs[0].id, "a"),
            _ => panic!("wrong record"),
        }
    }

    #[test]
    fn corrupt_rejected() {
        let entry = WalEntry {
            seq: 1,
            request_id: None,
            ts_ms: 0,
            record: WalRecord::Delete { ids: vec!["x".into()] },
        };
        let mut bytes = encode_entry(&entry).unwrap().to_vec();
        let n = bytes.len();
        bytes[n - 2] ^= 0x55;
        assert!(decode_entry(&bytes).is_err());
    }
}
