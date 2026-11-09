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

/// A single WAL record (one per document operation).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WalRecord {
    Upsert(Document),
    Delete { id: DocId },
}

/// A batch of WAL records written atomically.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalBatch {
    pub first_seq: u64,
    pub last_seq: u64,
    pub records: Vec<WalRecord>,
    #[serde(default)]
    pub deduplicated: bool,
}

impl WalBatch {
    pub fn new(first_seq: u64, records: Vec<WalRecord>) -> Self {
        let count = records.len() as u64;
        Self {
            first_seq,
            last_seq: if count > 0 { first_seq + count - 1 } else { first_seq },
            records,
            deduplicated: false,
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let body = serde_json::to_vec(self)?;
        Ok(framing::encode_framed(WAL_MAGIC, &body))
    }

    pub fn decode(data: &[u8]) -> Result<WalBatch> {
        let body = framing::decode_framed(WAL_MAGIC, data)?;
        Ok(serde_json::from_slice(body)?)
    }
}

// Legacy entry-level API (kept for backwards compatibility)
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
    fn batch_roundtrip() {
        let records = vec![
            WalRecord::Upsert(Document {
                id: "a".into(),
                text: BTreeMap::new(),
                vector: Some(vec![1.0, 2.0]),
                sparse_vector: None,
                attributes: BTreeMap::new(),
            }),
            WalRecord::Delete { id: "b".into() },
        ];
        let batch = WalBatch::new(7, records);
        assert_eq!(batch.first_seq, 7);
        assert_eq!(batch.last_seq, 8);
        let bytes = batch.encode().unwrap();
        let back = WalBatch::decode(&bytes).unwrap();
        assert_eq!(back.first_seq, 7);
        assert_eq!(back.records.len(), 2);
    }

    #[test]
    fn entry_roundtrip() {
        let entry = WalEntry {
            seq: 7,
            request_id: Some("req-1".into()),
            ts_ms: now_ms(),
            record: WalRecord::Upsert(Document {
                id: "a".into(),
                text: BTreeMap::new(),
                vector: Some(vec![1.0, 2.0]),
                sparse_vector: None,
                attributes: BTreeMap::new(),
            }),
        };
        let bytes = encode_entry(&entry).unwrap();
        let back = decode_entry(&bytes).unwrap();
        assert_eq!(back.seq, 7);
    }
}
