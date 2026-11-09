"""Deterministic synthetic dataset generator for the Lagoon benchmark suite.

Generates documents with:
  * dense vectors drawn from a Gaussian mixture (clustered, so IVF-style ANN
    indexes behave realistically — uniform random vectors make ANN look
    artificially easy or hard depending on dimension);
  * text fields built from a fixed vocabulary with a Zipf-like frequency
    distribution (so BM25 has realistic term statistics);
  * filterable attributes (category, year, price, in_stock).

Everything is seeded, so two runs with the same parameters produce identical
datasets — required for comparing results across machines or versions.

Usage:
    python datagen.py --docs 50000 --dim 256 --out data/bench-50k.jsonl
"""

from __future__ import annotations

import argparse
import json
import os
from typing import Iterator, List, Tuple

import numpy as np

# A fixed 256-word vocabulary.  Words are ordinary English nouns/adjectives;
# the generator samples them with Zipf weights so a few words are very common
# and the tail is rare, mimicking natural-language term distribution.
VOCAB: List[str] = [
    "harbor", "lantern", "granite", "meadow", "copper", "willow", "ember",
    "compass", "orchard", "thicket", "ledger", "anchor", "breeze", "canyon",
    "drift", "estuary", "fjord", "glacier", "hollow", "inlet", "juniper",
    "kelp", "lagoon", "marsh", "nectar", "oasis", "pebble", "quarry",
    "ridge", "summit", "tundra", "valley", "wharf", "yarrow", "zephyr",
    "basalt", "cedar", "delta", "ebb", "flint", "grove", "heath", "isle",
    "jetty", "knoll", "loch", "mesa", "narrows", "outcrop", "prairie",
    "quay", "reef", "shoal", "terrace", "upland", "vista", "weir", "atoll",
    "bluff", "cairn", "dune", "eddy", "ford", "gully", "haven", "islet",
    "jasper", "karst", "levee", "moor", "notch", "oxbow", "pass", "quicksand",
    "rapids", "strand", "trench", "undertow", "verge", "wash", "arroyo",
    "bayou", "cove", "divide", "escarpment", "floe", "geyser", "headland",
    "iceberg", "jungle", "key", "lowland", "moraine", "nook", "overhang",
    "plateau", "ravine", "spit", "tarn", "uplift", "vent", "waterfall",
    "ancient", "bright", "calm", "deep", "early", "faint", "gentle",
    "hidden", "inner", "jagged", "keen", "low", "misty", "narrow", "open",
    "pale", "quiet", "rough", "steep", "tall", "upper", "vast", "wide",
    "amber", "blue", "crimson", "dark", "emerald", "frost", "golden",
    "hazel", "ivory", "jade", "khaki", "lilac", "maroon", "navy", "olive",
    "pearl", "quartz", "russet", "silver", "teal", "umber", "violet",
    "white", "azure", "bronze", "coral", "dusty", "ecru", "fawn", "gray",
    "hoary", "indigo", "jet", "lemon", "mauve", "noir", "ochre", "plum",
    "rose", "sable", "tan", "ultramarine", "verdant", "wheat", "alder",
    "birch", "chestnut", "dogwood", "elm", "fir", "ginkgo", "hawthorn",
    "ironwood", "joshua", "katsura", "larch", "magnolia", "nutmeg", "oak",
    "pine", "quince", "redwood", "spruce", "tamarack", "umbrella", "vine",
    "walnut", "yew", "aspen", "beech", "cypress", "driftwood", "eucalyptus",
    "fern", "gorse", "holly", "ivy", "jacaranda", "kapok", "laurel",
    "maple", "nettle", "olive_tree", "poplar", "reed", "sequoia", "teak",
    "underbrush", "veld", "wisteria", "acacia", "bamboo", "clover",
    "daffodil", "edelweiss", "foxglove", "geranium", "heather", "iris",
    "jasmine", "kudzu", "lavender", "marigold", "narcissus", "orchid",
    "peony", "ranunculus", "saffron", "thistle", "umbel", "verbena",
    "wallflower", "yucca", "zinnia", "alpine", "boreal", "coastal",
    "desert", "estuarine", "fluvial", "glacial", "highland", "insular",
    "jagged_peak", "karstic", "littoral", "montane", "nival", "oceanic",
    "pelagic", "riparian", "subalpine", "tidal", "undersea", "volcanic",
    "wetland", "arid", "brackish", "cold", "damp", "elevated", "frozen",
]

CATEGORIES = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta"]


def make_mixture(
    rng: np.random.Generator, dim: int, n_clusters: int
) -> Tuple[np.ndarray, np.ndarray]:
    """Cluster centers and per-cluster scales."""
    centers = rng.normal(0.0, 1.0, size=(n_clusters, dim)).astype(np.float32)
    scales = rng.uniform(0.05, 0.25, size=n_clusters).astype(np.float32)
    return centers, scales


def generate_vectors(
    n: int, dim: int, seed: int, n_clusters: int = 64
) -> np.ndarray:
    rng = np.random.default_rng(seed)
    centers, scales = make_mixture(rng, dim, n_clusters)
    assignments = rng.integers(0, n_clusters, size=n)
    noise = rng.normal(0.0, 1.0, size=(n, dim)).astype(np.float32)
    vecs = centers[assignments] + noise * scales[assignments][:, None]
    # L2-normalise so cosine and dot product are well-behaved.
    norms = np.linalg.norm(vecs, axis=1, keepdims=True)
    norms[norms == 0] = 1.0
    return (vecs / norms).astype(np.float32)


def zipf_weights(n_words: int, exponent: float = 1.1) -> np.ndarray:
    ranks = np.arange(1, n_words + 1, dtype=np.float64)
    w = 1.0 / np.power(ranks, exponent)
    return w / w.sum()


def generate_text(rng: np.random.Generator, weights: np.ndarray, n_words: int) -> str:
    idx = rng.choice(len(VOCAB), size=n_words, p=weights)
    return " ".join(VOCAB[i] for i in idx)


def generate_documents(
    n_docs: int, dim: int, seed: int = 42
) -> Iterator[dict]:
    rng = np.random.default_rng(seed + 1)
    weights = zipf_weights(len(VOCAB))
    vectors = generate_vectors(n_docs, dim, seed)
    for i in range(n_docs):
        title_len = int(rng.integers(3, 8))
        body_len = int(rng.integers(30, 120))
        yield {
            "id": f"doc-{i:08d}",
            "vector": [round(float(x), 6) for x in vectors[i]],
            "fields": {
                "title": generate_text(rng, weights, title_len),
                "body": generate_text(rng, weights, body_len),
            },
            "attributes": {
                "category": CATEGORIES[int(rng.integers(0, len(CATEGORIES)))],
                "year": int(rng.integers(2000, 2025)),
                "price": round(float(rng.uniform(1.0, 500.0)), 2),
                "in_stock": bool(rng.integers(0, 2)),
            },
        }


def generate_query_vectors(n: int, dim: int, seed: int = 42) -> np.ndarray:
    """Query vectors drawn from the same mixture (different noise seed) so
    they have realistic proximity to the corpus."""
    return generate_vectors(n, dim, seed + 7919)


def generate_query_terms(n: int, seed: int = 42, words_per_query: int = 3) -> List[str]:
    rng = np.random.default_rng(seed + 104729)
    weights = zipf_weights(len(VOCAB))
    return [generate_text(rng, weights, words_per_query) for _ in range(n)]


def main() -> None:
    ap = argparse.ArgumentParser(description="Generate a benchmark dataset.")
    ap.add_argument("--docs", type=int, default=50_000)
    ap.add_argument("--dim", type=int, default=256)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--out", default="data/bench.jsonl")
    args = ap.parse_args()

    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    n_written = 0
    with open(args.out, "w", encoding="utf-8") as f:
        for doc in generate_documents(args.docs, args.dim, args.seed):
            f.write(json.dumps(doc) + "\n")
            n_written += 1
            if n_written % 10_000 == 0:
                print(f"  wrote {n_written}/{args.docs}")
    print(f"Wrote {n_written} documents ({args.dim}-d vectors) to {args.out}")


if __name__ == "__main__":
    main()
