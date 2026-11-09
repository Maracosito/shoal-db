//! Write-ahead log object format.
//!
//! Each committed write batch is exactly one immutable WAL object, created
//! atomically with `put_if_absent` at key `wal/<seq>.wal`. Because object
//! creation is the commit point, there is no partial-batch state: a WAL
//! object either exists in full or not at all. Per-object framing still
//! carries a CRC so we can detect bit rot and torn writes on backends with
//! weaker guarantees (e.g. a local filesystem written by a non-ReefDB tool).
//!
//! ## Wire format
//!
//! ```text
//! offset  size  field
//! 0       4     magic            b"RFW1"
//! 4       2     format version   u16 LE (currently 1)
//! 6       2     flags            u16 LE (reserved, 0)
//! 8       4     payload length   u32 LE
//! 12      N     payload          bincode(WalBatch)
//! 12+N    4     crc32(payload)   u32 LE
//! ```
//!
//! The decoder is strict: short buffers are [`EngineError::Incomplete`];
//! wrong magic/version, checksum mismatch, trailing bytes, or undecodable
//! payloads are [`EngineError::Corrupt`].
//!
//! ## Physical operations
//!
//! The write path resolves all *logical* semantics (patches, `CreateOnly`
//! upserts, write conditions) **before** logging, against the currently
//! visible state. The WAL therefore contains only physical operations —
//! `Put(full document)` and `Delete(id)` — which makes recovery replay a
//! deterministic, context-free fold. The original logical intent (e.g. which
//! batches were patches) is not needed for recovery and is not stored.

use crate::checksum::crc32;
use crate::error::{EngineError, EngineResult};
use bytes::{BufMut, Bytes, BytesMut};
use reef_types::{DocId, Document};
use serde::{Deserialize, Serialize};

pub const WAL_MAGIC: &[u8; 4] = b"RFW1";
pub const WAL_FORMAT_VERSION: u16 = 1;

/// Fixed framing overhead: 12-byte header + 4-byte trailer CRC.
pub const WAL_HEADER_LEN: usize = 12;
pub const WAL_TRAILER_LEN: usize = 4;

/// A physical (fully resolved) operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WalOp {
    /// Set the document to exactly this value (replaces any prior version).
    Put(Document),
    /// Remove the document (tombstone until compaction reclaims it).
    Delete(DocId),
}

impl WalOp {
    pub fn doc_id(&self) -> &DocId {
        match self {
            WalOp::Put(doc) => &doc.id,
            WalOp::Delete(id) => id,
        }
    }
}

/// The payload of one WAL object: one durably committed batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalBatch {
    /// WAL sequence number. Redundant with the object key; stored inside the
    /// payload as a cross-check against misplaced/renamed objects.
    pub seq: u64,
    /// Client idempotency key, if any. Recovery rebuilds the dedup window by
    /// replaying these.
    pub idempotency_key: Option<String>,
    /// Commit wall-clock time (ms since Unix epoch), informational.
    pub committed_at_ms: u64,
    /// Physical operations, applied in order within the batch.
    pub ops: Vec<WalOp>,
}

/// Encode a WAL batch into its framed object representation.
pub fn encode_wal_batch(batch: &WalBatch) -> EngineResult<Bytes> {
    let payload = bincode::serialize(batch)
        .map_err(|e| EngineError::Internal(format!("wal encode: {e}")))?;
    if payload.len() > u32::MAX as usize {
        return Err(EngineError::InvalidArgument(format!(
            "wal batch payload is {} bytes; max is {}",
            payload.len(),
            u32::MAX
        )));
    }
    let mut buf = BytesMut::with_capacity(WAL_HEADER_LEN + payload.len() + WAL_TRAILER_LEN);
    buf.put_slice(WAL_MAGIC);
    buf.put_u16_le(WAL_FORMAT_VERSION);
    buf.put_u16_le(0); // flags
    buf.put_u32_le(payload.len() as u32);
    buf.put_slice(&payload);
    buf.put_u32_le(crc32(&payload));
    Ok(buf.freeze())
}

/// Decode and verify a WAL object. `path` is used only for error reporting.
/// `expected_seq`, when provided, must match the sequence stored inside the
/// payload (catching objects copied to the wrong key).
pub fn decode_wal_batch(
    path: &str,
    buf: &[u8],
    expected_seq: Option<u64>,
) -> EngineResult<WalBatch> {
    if buf.len() < WAL_HEADER_LEN + WAL_TRAILER_LEN {
        return Err(EngineError::incomplete(
            path,
            format!(
                "object is {} bytes; minimum frame is {}",
                buf.len(),
                WAL_HEADER_LEN + WAL_TRAILER_LEN
            ),
        ));
    }
    if &buf[0..4] != WAL_MAGIC {
        return Err(EngineError::corrupt(
            path,
            format!("bad magic {:02x?}", &buf[0..4]),
        ));
    }
    let version = u16::from_le_bytes([buf[4], buf[5]]);
    if version != WAL_FORMAT_VERSION {
        return Err(EngineError::corrupt(
            path,
            format!("unsupported wal format version {version}"),
        ));
    }
    let payload_len = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
    let expected_total = WAL_HEADER_LEN + payload_len + WAL_TRAILER_LEN;
    if buf.len() < expected_total {
        return Err(EngineError::incomplete(
            path,
            format!(
                "object is {} bytes; frame declares {}",
                buf.len(),
                expected_total
            ),
        ));
    }
    if buf.len() > expected_total {
        return Err(EngineError::corrupt(
            path,
            format!(
                "object is {} bytes; frame declares {} (trailing garbage)",
                buf.len(),
                expected_total
            ),
        ));
    }
    let payload = &buf[WAL_HEADER_LEN..WAL_HEADER_LEN + payload_len];
    let stored_crc = u32::from_le_bytes([
        buf[expected_total - 4],
        buf[expected_total - 3],
        buf[expected_total - 2],
        buf[expected_total - 1],
    ]);
    let actual_crc = crc32(payload);
    if stored_crc != actual_crc {
        return Err(EngineError::corrupt(
            path,
            format!("crc mismatch: stored {stored_crc:#010x}, computed {actual_crc:#010x}"),
        ));
    }
    let batch: WalBatch = bincode::deserialize(payload)
        .map_err(|e| EngineError::corrupt(path, format!("payload decode failed: {e}")))?;
    if let Some(seq) = expected_seq {
        if batch.seq != seq {
            return Err(EngineError::corrupt(
                path,
                format!(
                    "sequence mismatch: key implies {seq}, payload says {}",
                    batch.seq
                ),
            ));
        }
    }
    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reef_types::Value;
    use std::collections::BTreeMap;

    fn sample_batch() -> WalBatch {
        let mut attrs = BTreeMap::new();
        attrs.insert("title".to_string(), Value::String("hello".to_string()));
        attrs.insert("rank".to_string(), Value::I64(3));
        WalBatch {
            seq: 42,
            idempotency_key: Some("client-key-1".to_string()),
            committed_at_ms: 1_700_000_000_000,
            ops: vec![
                WalOp::Put(Document {
                    id: DocId::new("doc-1").unwrap(),
                    vector: Some(vec![0.25, -1.5, 3.0]),
                    sparse: None,
                    attributes: attrs,
                }),
                WalOp::Delete(DocId::new("doc-2").unwrap()),
            ],
        }
    }

    #[test]
    fn roundtrip() {
        let batch = sample_batch();
        let encoded = encode_wal_batch(&batch).unwrap();
        let decoded = decode_wal_batch("wal/test", &encoded, Some(42)).unwrap();
        assert_eq!(batch, decoded);
    }

    #[test]
    fn seq_cross_check() {
        let encoded = encode_wal_batch(&sample_batch()).unwrap();
        // Without expected seq, decodes fine.
        assert!(decode_wal_batch("p", &encoded, None).is_ok());
        // With wrong expected seq, fails as corrupt.
        let err = decode_wal_batch("p", &encoded, Some(7)).unwrap_err();
        assert!(matches!(err, EngineError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn detects_bit_flip_in_payload() {
        let encoded = encode_wal_batch(&sample_batch()).unwrap();
        let mut bad = encoded.to_vec();
        let mid = WAL_HEADER_LEN + 5;
        bad[mid] ^= 0xFF;
        let err = decode_wal_batch("p", &bad, Some(42)).unwrap_err();
        assert!(matches!(err, EngineError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn detects_truncation_as_incomplete() {
        let encoded = encode_wal_batch(&sample_batch()).unwrap();
        // Truncated mid-payload.
        let err = decode_wal_batch("p", &encoded[..encoded.len() - 10], Some(42)).unwrap_err();
        assert!(matches!(err, EngineError::Incomplete { .. }), "{err}");
        // Truncated below header size.
        let err = decode_wal_batch("p", &encoded[..6], Some(42)).unwrap_err();
        assert!(matches!(err, EngineError::Incomplete { .. }), "{err}");
        // Empty.
        let err = decode_wal_batch("p", &[], Some(42)).unwrap_err();
        assert!(matches!(err, EngineError::Incomplete { .. }), "{err}");
    }

    #[test]
    fn detects_trailing_garbage() {
        let encoded = encode_wal_batch(&sample_batch()).unwrap();
        let mut padded = encoded.to_vec();
        padded.extend_from_slice(b"junk");
        let err = decode_wal_batch("p", &padded, Some(42)).unwrap_err();
        assert!(matches!(err, EngineError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn detects_bad_magic_and_version() {
        let encoded = encode_wal_batch(&sample_batch()).unwrap();
        let mut bad_magic = encoded.to_vec();
        bad_magic[0] = b'X';
        assert!(matches!(
            decode_wal_batch("p", &bad_magic, None).unwrap_err(),
            EngineError::Corrupt { .. }
        ));

        let mut bad_version = encoded.to_vec();
        bad_version[4] = 0xEE;
        assert!(matches!(
            decode_wal_batch("p", &bad_version, None).unwrap_err(),
            EngineError::Corrupt { .. }
        ));
    }

    #[test]
    fn detects_corrupted_crc_trailer() {
        let encoded = encode_wal_batch(&sample_batch()).unwrap();
        let mut bad = encoded.to_vec();
        let last = bad.len() - 1;
        bad[last] ^= 0x01;
        assert!(matches!(
            decode_wal_batch("p", &bad, None).unwrap_err(),
            EngineError::Corrupt { .. }
        ));
    }

    #[test]
    fn empty_ops_batch_roundtrips() {
        // The engine never writes empty batches, but the format must not
        // choke on them (forward compatibility for control records).
        let batch = WalBatch {
            seq: 1,
            idempotency_key: None,
            committed_at_ms: 0,
            ops: vec![],
        };
        let encoded = encode_wal_batch(&batch).unwrap();
        assert_eq!(decode_wal_batch("p", &encoded, Some(1)).unwrap(), batch);
    }
}
