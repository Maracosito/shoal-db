//! Dense-vector search: math primitives, exact kNN baseline, k-means
//! training, and the object-storage-friendly IVF-Flat approximate index.

pub mod exact;
pub mod ivf;
pub mod kmeans;
pub mod math;

pub use math::Metric;
