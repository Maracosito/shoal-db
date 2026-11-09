use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::{Result, ShoalError};
use crate::filter::Filter;

pub type DocId = String;

/// Milliseconds since the unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A metadata attribute value. JSON representation is untagged:
/// `null`, `true`, `42`, `4.2`, `"s"`, `["a","b"]`.
///
/// NOTE: untagged enums require a self-describing format, so all persistent
/// encodings in Shoal (WAL, segments, manifests) use JSON bodies inside a
/// CRC-checked binary frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttributeValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    StringList(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SparseVector {
    pub indices: Vec<u32>,
    pub values: Vec<f32>,
}

impl SparseVector {
    /// Validate and normalize: indices/values same length, sorted by index,
    /// no duplicate indices.
    pub fn normalize(&mut self) -> Result<()> {
        if self.indices.len() != self.values.len() {
            return Err(ShoalError::InvalidRequest(
                "sparse vector indices and values must have equal length".into(),
            ));
        }
        let mut pairs: Vec<(u32, f32)> = self
            .indices
            .iter()
            .copied()
            .zip(self.values.iter().copied())
            .collect();
        pairs.sort_by_key(|(i, _)| *i);
        for w in pairs.windows(2) {
            if w[0].0 == w[1].0 {
                return Err(ShoalError::InvalidRequest(format!(
                    "duplicate sparse index {}",
                    w[0].0
                )));
            }
        }
        self.indices = pairs.iter().map(|(i, _)| *i).collect();
        self.values = pairs.iter().map(|(_, v)| *v).collect();
        Ok(())
    }

    /// Dot product of two sparse vectors (both must be index-sorted).
    pub fn dot(&self, other: &SparseVector) -> f32 {
        let (mut a, mut b, mut s) = (0usize, 0usize, 0f32);
        while a < self.indices.len() && b < other.indices.len() {
            match self.indices[a].cmp(&other.indices[b]) {
                std::cmp::Ordering::Less => a += 1,
                std::cmp::Ordering::Greater => b += 1,
                std::cmp::Ordering::Equal => {
                    s += self.values[a] * other.values[b];
                    a += 1;
                    b += 1;
                }
            }
        }
        s
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: DocId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparse_vector: Option<SparseVector>,
    #[serde(default)]
    pub attributes: BTreeMap<String, AttributeValue>,
}

/// A partial document update. `attributes` are merged into the existing
/// document; an attribute set to `null` is removed. A present `vector` /
/// `sparse_vector` replaces the existing one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PatchDoc {
    pub id: DocId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparse_vector: Option<SparseVector>,
    #[serde(default)]
    pub attributes: BTreeMap<String, AttributeValue>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistanceMetric {
    #[default]
    Cosine,
    Dot,
    Euclidean,
}

impl DistanceMetric {
    /// Similarity score where higher is better (Euclidean is negated distance).
    pub fn score(&self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            DistanceMetric::Cosine => {
                let mut dot = 0f32;
                let mut na = 0f32;
                let mut nb = 0f32;
                for (x, y) in a.iter().zip(b.iter()) {
                    dot += x * y;
                    na += x * x;
                    nb += y * y;
                }
                let denom = na.sqrt() * nb.sqrt();
                if denom <= f32::EPSILON {
                    0.0
                } else {
                    dot / denom
                }
            }
            DistanceMetric::Dot => a.iter().zip(b.iter()).map(|(x, y)| x * y).sum(),
            DistanceMetric::Euclidean => {
                let d2: f32 = a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum();
                -d2.sqrt()
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NamespaceConfig {
    /// Enforced dense vector dimension. If unset, adopted from the first
    /// upserted vector and persisted at the next flush.
    #[serde(default)]
    pub vector_dim: Option<u32>,
    #[serde(default)]
    pub metric: DistanceMetric,
}

fn default_rrf_k() -> f32 {
    60.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Fusion {
    Rrf {
        #[serde(default = "default_rrf_k")]
        k: f32,
    },
    Weighted {
        vector_weight: f32,
        text_weight: f32,
    },
}

fn default_top_k() -> usize {
    10
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparse_vector: Option<SparseVector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// BM25 field weights, e.g. `{"title": 2.0, "body": 1.0}`. Defaults to
    /// every indexed text field with weight 1.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_fields: Option<BTreeMap<String, f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Filter>,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_attributes: Option<Vec<String>>,
    #[serde(default)]
    pub include_vectors: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fusion: Option<Fusion>,
}

impl Default for QueryRequest {
    fn default() -> Self {
        QueryRequest {
            vector: None,
            sparse_vector: None,
            text: None,
            text_fields: None,
            filter: None,
            top_k: default_top_k(),
            include_attributes: None,
            include_vectors: false,
            fusion: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryHit {
    pub id: DocId,
    pub score: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_score: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_score: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    #[serde(default)]
    pub attributes: BTreeMap<String, AttributeValue>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryResponse {
    pub hits: Vec<QueryHit>,
    /// Execution plan chosen by the (baseline) planner, e.g. `exact_knn+filter`.
    pub plan: String,
    pub took_ms: u64,
}

/// Options applied to a write batch.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WriteOptions {
    /// Client-supplied idempotency token. Re-sending a request with the same
    /// token within the retention window is a no-op.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Conditional write: fails with 409 unless the namespace data version
    /// matches exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_version: Option<u64>,
}
