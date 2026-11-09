"""Deterministic, dependency-free demo embeddings.

The Shoal database is embedding-provider agnostic; it stores and searches
whatever vectors you give it. For a self-contained demo that runs offline,
we use a simple feature-hashing embedding: each token is hashed into one of
``DIM`` buckets with a +/- sign, the bucket counts are accumulated, and the
vector is L2-normalised.

This is NOT a semantic embedding model — it behaves like a bag-of-words
signature — but it is deterministic, fast, vendor-free, and good enough to
demonstrate vector search, hybrid fusion, and filters end to end. Swap in a
real embedding provider (OpenAI, Cohere, sentence-transformers, ...) by
replacing :func:`embed`.
"""

from __future__ import annotations

import hashlib
import math
import re

DIM = 64

_TOKEN = re.compile(r"[a-z0-9]+")


def tokenize(text: str) -> list[str]:
    return _TOKEN.findall(text.lower())


def embed(text: str) -> list[float]:
    vec = [0.0] * DIM
    for tok in tokenize(text):
        digest = hashlib.md5(tok.encode("utf-8")).digest()
        idx = int.from_bytes(digest[:4], "big") % DIM
        sign = 1.0 if digest[4] % 2 == 0 else -1.0
        vec[idx] += sign
    norm = math.sqrt(sum(x * x for x in vec))
    if norm > 0.0:
        vec = [x / norm for x in vec]
    return vec
