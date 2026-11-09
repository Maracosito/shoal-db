//! Recall@k validation of the IVF ANN index against the exact kNN
//! baseline, plus equivalence tests for the storage-backed (range-read)
//! search path. These are the acceptance tests for the milestone's
//! "recall-vs-nprobe tuning" requirement.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use roaring::RoaringBitmap;

use shoal_query::types::DocOrd;
use shoal_query::vector::exact;
use shoal_query::vector::ivf::{
    decode_posting_list, recall_at_k, search_with_meta, IvfConfig, IvfIndex, IvfMeta, PostingFetch,
    PostingList,
};
use shoal_query::vector::math::Metric;
use shoal_query::Result;

/// Standard normal sample via Box-Muller.
fn gauss(rng: &mut StdRng) -> f32 {
    let u1: f32 = rng.gen::<f32>().max(1e-7);
    let u2: f32 = rng.gen();
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos()
}

struct Dataset {
    dim: usize,
    ords: Vec<DocOrd>,
    data: Vec<f32>,
    queries: Vec<Vec<f32>>,
}

/// Clustered synthetic dataset: `n` points around `n_centers` Gaussian
/// centers; queries are drawn near random centers (the realistic case for
/// embedding workloads).
fn make_dataset(seed: u64, n: usize, dim: usize, n_centers: usize, n_queries: usize) -> Dataset {
    let mut rng = StdRng::seed_from_u64(seed);
    let centers: Vec<Vec<f32>> = (0..n_centers)
        .map(|_| (0..dim).map(|_| gauss(&mut rng) * 5.0 + 2.0).collect())
        .collect();
    let mut data = Vec::with_capacity(n * dim);
    for i in 0..n {
        let c = &centers[i % n_centers];
        for j in 0..dim {
            data.push(c[j] + gauss(&mut rng) * 0.8);
        }
    }
    let queries = (0..n_queries)
        .map(|_| {
            let c = &centers[rng.gen_range(0..n_centers)];
            (0..dim).map(|j| c[j] + gauss(&mut rng) * 0.8).collect()
        })
        .collect();
    Dataset {
        dim,
        ords: (0..n as u32).collect(),
        data,
        queries,
    }
}

fn mean_recall(
    ds: &Dataset,
    idx: &IvfIndex,
    metric: Metric,
    k: usize,
    nprobe: usize,
) -> f64 {
    let mut total = 0.0;
    for q in &ds.queries {
        let exact = exact::search_flat(metric, q, ds.dim, &ds.ords, &ds.data, k, None).unwrap();
        let approx = idx.search(q, k, nprobe, None).unwrap();
        total += recall_at_k(&approx, &exact, k);
    }
    total / ds.queries.len() as f64
}

#[test]
fn euclidean_recall_improves_with_nprobe_and_full_probe_is_exact() {
    let ds = make_dataset(101, 4000, 32, 40, 25);
    let idx = IvfIndex::build(
        Metric::Euclidean,
        ds.dim,
        &ds.ords,
        &ds.data,
        &IvfConfig::default(),
    )
    .unwrap();
    let k = 10;

    let r1 = mean_recall(&ds, &idx, Metric::Euclidean, k, 1);
    let r8 = mean_recall(&ds, &idx, Metric::Euclidean, k, 8);
    let r16 = mean_recall(&ds, &idx, Metric::Euclidean, k, 16);
    let rfull = mean_recall(&ds, &idx, Metric::Euclidean, k, idx.nlist);

    eprintln!(
        "euclidean recall@{k}: nprobe=1 {r1:.3}, 8 {r8:.3}, 16 {r16:.3}, full {rfull:.3} (nlist={})",
        idx.nlist
    );
    // IVF-Flat with a full probe scans every vector with identical
    // arithmetic to the exact path, so recall must be 1.0 exactly.
    assert_eq!(rfull, 1.0);
    assert!(r8 >= 0.8, "recall@10 at nprobe=8 too low: {r8}");
    assert!(r16 >= r1, "recall must not degrade as nprobe grows");
}

#[test]
fn cosine_recall_tuning() {
    let ds = make_dataset(202, 4000, 32, 40, 25);
    let idx = IvfIndex::build(
        Metric::Cosine,
        ds.dim,
        &ds.ords,
        &ds.data,
        &IvfConfig::default(),
    )
    .unwrap();
    let k = 10;

    let r1 = mean_recall(&ds, &idx, Metric::Cosine, k, 1);
    let r16 = mean_recall(&ds, &idx, Metric::Cosine, k, 16);
    let rfull = mean_recall(&ds, &idx, Metric::Cosine, k, idx.nlist);

    eprintln!(
        "cosine recall@{k}: nprobe=1 {r1:.3}, 16 {r16:.3}, full {rfull:.3} (nlist={})",
        idx.nlist
    );
    // The cosine scan path uses pre-normalized vectors (dot product) while
    // the exact baseline divides by norms at query time; tiny float
    // differences can swap near-tied boundary candidates, so we allow a
    // hair below perfect at full probe.
    assert!(rfull >= 0.95, "full-probe cosine recall too low: {rfull}");
    assert!(r16 >= 0.8, "recall@10 at nprobe=16 too low: {r16}");
    assert!(r16 >= r1);
}

#[test]
fn dot_product_recall_tuning() {
    let ds = make_dataset(303, 4000, 32, 40, 25);
    let idx = IvfIndex::build(
        Metric::Dot,
        ds.dim,
        &ds.ords,
        &ds.data,
        &IvfConfig::default(),
    )
    .unwrap();
    let k = 10;

    let r1 = mean_recall(&ds, &idx, Metric::Dot, k, 1);
    let r16 = mean_recall(&ds, &idx, Metric::Dot, k, 16);
    let rfull = mean_recall(&ds, &idx, Metric::Dot, k, idx.nlist);

    eprintln!(
        "dot recall@{k}: nprobe=1 {r1:.3}, 16 {r16:.3}, full {rfull:.3} (nlist={})",
        idx.nlist
    );
    // MIPS over L2-trained centroids is the weakest probing geometry; we
    // assert exactness at full probe and a meaningful (honest) lower bound
    // at moderate nprobe rather than an inflated figure.
    assert_eq!(rfull, 1.0);
    assert!(r16 >= 0.3, "recall@10 at nprobe=16 too low: {r16}");
    assert!(r16 >= r1);
}

#[test]
fn filtered_ann_full_probe_matches_filtered_exact() {
    let ds = make_dataset(404, 2000, 24, 25, 10);
    let idx = IvfIndex::build(
        Metric::Euclidean,
        ds.dim,
        &ds.ords,
        &ds.data,
        &IvfConfig::default(),
    )
    .unwrap();
    let mut allow = RoaringBitmap::new();
    for &o in ds.ords.iter().filter(|o| *o % 2 == 0) {
        allow.insert(o);
    }
    for q in &ds.queries {
        let exact =
            exact::search_flat(Metric::Euclidean, q, ds.dim, &ds.ords, &ds.data, 10, Some(&allow))
                .unwrap();
        let approx = idx.search(q, 10, idx.nlist, Some(&allow)).unwrap();
        assert!(approx.iter().all(|n| n.ord % 2 == 0));
        let a: Vec<DocOrd> = approx.iter().map(|n| n.ord).collect();
        let e: Vec<DocOrd> = exact.iter().map(|n| n.ord).collect();
        assert_eq!(a, e, "filtered full-probe IVF must equal filtered exact");
    }
}

/// Simulates object-storage range reads: the fetcher holds the raw bytes
/// and the cached prefix, and serves each posting list by slicing exactly
/// the byte range the directory points at.
struct SliceFetcher<'a> {
    bytes: &'a [u8],
    meta: &'a IvfMeta,
}

impl PostingFetch for SliceFetcher<'_> {
    fn fetch_list(&self, list_id: u32) -> Result<PostingList> {
        let entry = &self.meta.directory[list_id as usize];
        let start = entry.offset as usize;
        let end = start + entry.byte_len as usize;
        decode_posting_list(&self.bytes[start..end], self.meta.header.dim, entry)
    }
}

#[test]
fn storage_backed_search_equals_in_memory_search() {
    let ds = make_dataset(505, 1500, 16, 20, 12);
    for metric in [Metric::Cosine, Metric::Dot, Metric::Euclidean] {
        let idx =
            IvfIndex::build(metric, ds.dim, &ds.ords, &ds.data, &IvfConfig::default()).unwrap();
        let bytes = idx.to_bytes();
        let meta = IvfMeta::from_prefix(&bytes).unwrap();
        let fetcher = SliceFetcher { bytes: &bytes, meta: &meta };
        for q in &ds.queries {
            let mem = idx.search(q, 10, 8, None).unwrap();
            let stored = search_with_meta(&meta, &fetcher, q, 10, 8, None).unwrap();
            assert_eq!(mem.len(), stored.len());
            for (a, b) in mem.iter().zip(stored.iter()) {
                assert_eq!(a.ord, b.ord, "metric {metric:?}");
                assert!((a.score - b.score).abs() < 1e-6);
            }
        }
    }
}

#[test]
fn round_tripped_index_returns_identical_results() {
    let ds = make_dataset(606, 1000, 16, 15, 8);
    let idx = IvfIndex::build(
        Metric::Euclidean,
        ds.dim,
        &ds.ords,
        &ds.data,
        &IvfConfig::default(),
    )
    .unwrap();
    let back = IvfIndex::from_bytes(&idx.to_bytes()).unwrap();
    for q in &ds.queries {
        let a = idx.search(q, 10, 6, None).unwrap();
        let b = back.search(q, 10, 6, None).unwrap();
        assert_eq!(
            a.iter().map(|n| n.ord).collect::<Vec<_>>(),
            b.iter().map(|n| n.ord).collect::<Vec<_>>()
        );
    }
}
