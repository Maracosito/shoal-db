//! Write operations: batches, upserts, patches, deletes, conditions, and the
//! column-oriented ingestion path.

use crate::document::{validate_attr_name, DocId, Document, SparseVector};
use crate::errors::ValidationError;
use crate::value::Value;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Maximum number of operations in a single write batch.
pub const MAX_BATCH_OPS: usize = 100_000;

/// Maximum idempotency key length in bytes.
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;

/// How an upsert behaves when the document already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpsertMode {
    /// Replace any existing document with the same ID (default).
    Overwrite,
    /// Fail the batch if a document with this ID already exists.
    CreateOnly,
}

impl Default for UpsertMode {
    fn default() -> Self {
        UpsertMode::Overwrite
    }
}

/// Dense-vector component of a patch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorPatch {
    /// Leave the existing vector untouched (default).
    Keep,
    /// Replace the vector.
    Set(Vec<f32>),
    /// Remove the vector.
    Clear,
}

impl Default for VectorPatch {
    fn default() -> Self {
        VectorPatch::Keep
    }
}

/// Sparse-vector component of a patch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SparsePatch {
    Keep,
    Set(SparseVector),
    Clear,
}

impl Default for SparsePatch {
    fn default() -> Self {
        SparsePatch::Keep
    }
}

/// A partial update of an existing document. Patching a document that does
/// not exist fails the batch with a not-found error: ReefDB does not
/// upsert-on-patch, so callers always know whether they created or modified.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatchSpec {
    pub id: DocId,
    /// Attributes to set (overwriting any existing value).
    #[serde(default)]
    pub set: BTreeMap<String, Value>,
    /// Attribute names to remove.
    #[serde(default)]
    pub unset: Vec<String>,
    #[serde(default)]
    pub vector: VectorPatch,
    #[serde(default)]
    pub sparse: SparsePatch,
}

impl PatchSpec {
    pub fn new(id: DocId) -> Self {
        PatchSpec {
            id,
            set: BTreeMap::new(),
            unset: Vec::new(),
            vector: VectorPatch::Keep,
            sparse: SparsePatch::Keep,
        }
    }

    pub fn set_attr(mut self, name: impl Into<String>, value: Value) -> Self {
        self.set.insert(name.into(), value);
        self
    }

    pub fn unset_attr(mut self, name: impl Into<String>) -> Self {
        self.unset.push(name.into());
        self
    }

    pub fn set_vector(mut self, v: Vec<f32>) -> Self {
        self.vector = VectorPatch::Set(v);
        self
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        for name in self.set.keys() {
            validate_attr_name(name)?;
        }
        for name in &self.unset {
            validate_attr_name(name)?;
            if self.set.contains_key(name) {
                return Err(ValidationError::InvalidPatch(format!(
                    "attribute {name:?} appears in both set and unset"
                )));
            }
        }
        if let VectorPatch::Set(v) = &self.vector {
            if v.is_empty() {
                return Err(ValidationError::InvalidVector(
                    "patched vector must not be empty (use clear)".to_string(),
                ));
            }
            if v.iter().any(|x| !x.is_finite()) {
                return Err(ValidationError::InvalidVector(
                    "patched vector components must be finite".to_string(),
                ));
            }
        }
        if let SparsePatch::Set(s) = &self.sparse {
            s.validate()?;
        }
        if self.set.is_empty()
            && self.unset.is_empty()
            && self.vector == VectorPatch::Keep
            && self.sparse == SparsePatch::Keep
        {
            return Err(ValidationError::InvalidPatch(
                "patch makes no changes".to_string(),
            ));
        }
        Ok(())
    }
}

/// A single logical write operation, as submitted by clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteOp {
    Upsert {
        doc: Document,
        #[serde(default)]
        mode: UpsertMode,
    },
    Patch(PatchSpec),
    Delete {
        id: DocId,
    },
}

impl WriteOp {
    pub fn upsert(doc: Document) -> Self {
        WriteOp::Upsert {
            doc,
            mode: UpsertMode::Overwrite,
        }
    }

    pub fn doc_id(&self) -> &DocId {
        match self {
            WriteOp::Upsert { doc, .. } => &doc.id,
            WriteOp::Patch(p) => &p.id,
            WriteOp::Delete { id } => id,
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            WriteOp::Upsert { doc, .. } => doc.validate(),
            WriteOp::Patch(p) => p.validate(),
            WriteOp::Delete { .. } => Ok(()),
        }
    }
}

/// A precondition checked atomically before the batch is committed. If the
/// condition fails, no operation in the batch is applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteCondition {
    /// The namespace version (its committed WAL sequence number) must equal
    /// this value. Enables optimistic concurrency control: read the version,
    /// compute, write back conditionally.
    NamespaceVersionEq(u64),
}

/// An atomic batch of write operations. The whole batch becomes one WAL
/// object: either every operation is durable, or none is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteBatch {
    pub ops: Vec<WriteOp>,
    /// Optional client-chosen idempotency key. Retrying a batch with the same
    /// key is a no-op that returns the original result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<WriteCondition>,
}

impl WriteBatch {
    pub fn new(ops: Vec<WriteOp>) -> Self {
        WriteBatch {
            ops,
            idempotency_key: None,
            condition: None,
        }
    }

    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    pub fn with_condition(mut self, cond: WriteCondition) -> Self {
        self.condition = Some(cond);
        self
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.ops.is_empty() {
            return Err(ValidationError::EmptyBatch);
        }
        if self.ops.len() > MAX_BATCH_OPS {
            return Err(ValidationError::BatchTooLarge {
                got: self.ops.len(),
                max: MAX_BATCH_OPS,
            });
        }
        if let Some(key) = &self.idempotency_key {
            if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES {
                return Err(ValidationError::InvalidIdempotencyKey(format!(
                    "key must be 1..={MAX_IDEMPOTENCY_KEY_BYTES} bytes"
                )));
            }
            if key.chars().any(|c| c.is_control()) {
                return Err(ValidationError::InvalidIdempotencyKey(
                    "key must not contain control characters".to_string(),
                ));
            }
        }
        for op in &self.ops {
            op.validate()?;
        }
        Ok(())
    }
}

/// Column-oriented ingestion: parallel arrays keyed by position. This is the
/// efficient bulk-load shape (one allocation per column instead of per
/// document) and converts losslessly into row [`WriteOp::Upsert`]s.
///
/// `Value::Null` entries in an attribute column mean "no value for this
/// document"; the attribute is simply absent from that document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnarBatch {
    pub ids: Vec<DocId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vectors: Option<Vec<Option<Vec<f32>>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparse: Option<Vec<Option<SparseVector>>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Vec<Value>>,
}

impl ColumnarBatch {
    /// Convert into row-oriented write operations, validating column lengths
    /// and each resulting document.
    pub fn into_write_ops(self, mode: UpsertMode) -> Result<Vec<WriteOp>, ValidationError> {
        let n = self.ids.len();
        if n == 0 {
            return Err(ValidationError::EmptyBatch);
        }
        if let Some(vs) = &self.vectors {
            if vs.len() != n {
                return Err(ValidationError::LengthMismatch(format!(
                    "vectors column has {} entries but there are {} ids",
                    vs.len(),
                    n
                )));
            }
        }
        if let Some(sp) = &self.sparse {
            if sp.len() != n {
                return Err(ValidationError::LengthMismatch(format!(
                    "sparse column has {} entries but there are {} ids",
                    sp.len(),
                    n
                )));
            }
        }
        for (name, col) in &self.attributes {
            validate_attr_name(name)?;
            if col.len() != n {
                return Err(ValidationError::LengthMismatch(format!(
                    "attribute column {name:?} has {} entries but there are {} ids",
                    col.len(),
                    n
                )));
            }
        }

        let ColumnarBatch {
            ids,
            vectors,
            sparse,
            attributes,
        } = self;

        let mut docs: Vec<Document> = ids.into_iter().map(Document::new).collect();
        if let Some(vs) = vectors {
            for (doc, v) in docs.iter_mut().zip(vs) {
                doc.vector = v;
            }
        }
        if let Some(sp) = sparse {
            for (doc, s) in docs.iter_mut().zip(sp) {
                doc.sparse = s;
            }
        }
        for (name, col) in attributes {
            for (doc, value) in docs.iter_mut().zip(col) {
                if !value.is_null() {
                    doc.attributes.insert(name.clone(), value);
                }
            }
        }

        let mut ops = Vec::with_capacity(docs.len());
        for doc in docs {
            doc.validate()?;
            ops.push(WriteOp::Upsert { doc, mode });
        }
        Ok(ops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> DocId {
        DocId::new(s).unwrap()
    }

    #[test]
    fn batch_validation() {
        let batch = WriteBatch::new(vec![WriteOp::upsert(Document::new(id("a")))]);
        assert!(batch.validate().is_ok());

        assert_eq!(
            WriteBatch::new(vec![]).validate(),
            Err(ValidationError::EmptyBatch)
        );

        let bad_key = WriteBatch::new(vec![WriteOp::Delete { id: id("a") }])
            .with_idempotency_key("x".repeat(300));
        assert!(bad_key.validate().is_err());
    }

    #[test]
    fn patch_validation() {
        let ok = PatchSpec::new(id("a")).set_attr("k", Value::I64(1));
        assert!(ok.validate().is_ok());

        let noop = PatchSpec::new(id("a"));
        assert!(noop.validate().is_err());

        let overlap = PatchSpec::new(id("a"))
            .set_attr("k", Value::I64(1))
            .unset_attr("k");
        assert!(overlap.validate().is_err());

        let mut bad_vec = PatchSpec::new(id("a"));
        bad_vec.vector = VectorPatch::Set(vec![]);
        assert!(bad_vec.validate().is_err());
    }

    #[test]
    fn columnar_conversion() {
        let mut attrs = BTreeMap::new();
        attrs.insert(
            "title".to_string(),
            vec![
                Value::String("first".to_string()),
                Value::Null,
                Value::String("third".to_string()),
            ],
        );
        attrs.insert(
            "rank".to_string(),
            vec![Value::I64(1), Value::I64(2), Value::I64(3)],
        );
        let batch = ColumnarBatch {
            ids: vec![id("a"), id("b"), id("c")],
            vectors: Some(vec![Some(vec![1.0, 0.0]), None, Some(vec![0.0, 1.0])]),
            sparse: None,
            attributes: attrs,
        };
        let ops = batch.into_write_ops(UpsertMode::Overwrite).unwrap();
        assert_eq!(ops.len(), 3);
        match &ops[1] {
            WriteOp::Upsert { doc, .. } => {
                assert_eq!(doc.id.as_str(), "b");
                assert!(doc.vector.is_none());
                // Null title was skipped; rank present.
                assert!(!doc.attributes.contains_key("title"));
                assert_eq!(doc.attributes.get("rank"), Some(&Value::I64(2)));
            }
            other => panic!("expected upsert, got {other:?}"),
        }
    }

    #[test]
    fn columnar_length_mismatch() {
        let batch = ColumnarBatch {
            ids: vec![id("a"), id("b")],
            vectors: Some(vec![Some(vec![1.0])]),
            sparse: None,
            attributes: BTreeMap::new(),
        };
        assert!(matches!(
            batch.into_write_ops(UpsertMode::Overwrite),
            Err(ValidationError::LengthMismatch(_))
        ));
    }
}
