"""Shared data and helpers for the live-server e2e suite.

Everything here is deliberately defensive about the exact shapes returned by
the SDK (model objects vs. plain dicts) so the assertions test *behaviour*,
not incidental representation details.
"""

from __future__ import annotations

import os
import time
import uuid
from typing import Any, Callable, Dict, List

E2E_URL = os.environ.get("SHOAL_E2E_URL", "").rstrip("/")
E2E_API_KEY = os.environ.get("SHOAL_E2E_API_KEY", "dev-root-key")

DIM = 4
NATURE_QUERY = [1.0, 0.0, 0.0, 0.0]
TECH_QUERY = [0.0, 1.0, 0.0, 0.0]
COOKING_QUERY = [0.0, 0.0, 1.0, 0.0]

# A tiny, deterministic corpus. Vectors live near the four axes so nearest-
# neighbour expectations are unambiguous regardless of distance metric.
CORPUS: List[Dict[str, Any]] = [
    {
        "id": "doc-1",
        "vector": [1.0, 0.0, 0.0, 0.0],
        "attributes": {
            "title": "Coral reef ecosystems",
            "body": "Coral reefs host shoals of vibrant fish in warm shallow ocean water.",
            "genre": "nature",
            "year": 2019,
            "rating": 4.5,
            "tags": ["ocean", "fish"],
        },
    },
    {
        "id": "doc-2",
        "vector": [0.9, 0.1, 0.0, 0.0],
        "attributes": {
            "title": "Mangrove forests",
            "body": "Mangroves shelter coastal nurseries where juvenile fish grow.",
            "genre": "nature",
            "year": 2021,
            "rating": 4.2,
            "tags": ["ocean", "trees"],
        },
    },
    {
        "id": "doc-3",
        "vector": [0.0, 1.0, 0.0, 0.0],
        "attributes": {
            "title": "Vector databases explained",
            "body": "Approximate nearest neighbour indexes make embedding search fast.",
            "genre": "tech",
            "year": 2023,
            "rating": 4.8,
            "tags": ["search", "ai"],
        },
    },
    {
        "id": "doc-4",
        "vector": [0.1, 0.9, 0.0, 0.0],
        "attributes": {
            "title": "BM25 ranking deep dive",
            "body": "An inverted index with term frequency saturation powers lexical ranking.",
            "genre": "tech",
            "year": 2022,
            "rating": 4.0,
            "tags": ["search", "text"],
        },
    },
    {
        "id": "doc-5",
        "vector": [0.0, 0.0, 1.0, 0.0],
        "attributes": {
            "title": "Sourdough basics",
            "body": "Flour, water, and a lively starter ferment into rustic bread.",
            "genre": "cooking",
            "year": 2020,
            "rating": 3.9,
            "tags": ["bread"],
        },
    },
    {
        "id": "doc-6",
        "vector": [0.0, 0.1, 0.9, 0.0],
        "attributes": {
            "title": "Ramen broth",
            "body": "Simmer bones for hours to extract deep umami for the noodles.",
            "genre": "cooking",
            "year": 2018,
            "rating": 4.6,
            "tags": ["soup"],
        },
    },
    {
        "id": "doc-7",
        "vector": [0.0, 0.0, 0.0, 1.0],
        "attributes": {
            "title": "The silk road",
            "body": "Caravans carried silk and ideas along ancient trade routes across Asia.",
            "genre": "history",
            "year": 2015,
            "rating": 4.1,
            "tags": ["trade"],
        },
    },
    {
        "id": "doc-8",
        "vector": [0.1, 0.0, 0.0, 0.9],
        "attributes": {
            "title": "Age of sail",
            "body": "Tall ships crossed the ocean guided by stars and sextants.",
            "genre": "history",
            "year": 2016,
            "rating": 3.7,
            "tags": ["ocean", "ships"],
        },
    },
]


def corpus_ids() -> List[str]:
    return [d["id"] for d in CORPUS]


def corpus_documents() -> list:
    """Build SDK Document models from the raw corpus."""
    from shoal import Document

    return [
        Document(id=d["id"], vector=d["vector"], attributes=d["attributes"])
        for d in CORPUS
    ]


def unique_ns(prefix: str = "e2e-py") -> str:
    return f"{prefix}-{uuid.uuid4().hex[:10]}"


# ---------------------------------------------------------------------------
# Shape-tolerant accessors
# ---------------------------------------------------------------------------

def _get(row: Any, key: str, default: Any = None) -> Any:
    if isinstance(row, dict):
        return row.get(key, default)
    return getattr(row, key, default)


def result_rows(res: Any) -> list:
    rows = getattr(res, "results", None)
    if rows is None:
        rows = res
    return list(rows)


def result_ids(res: Any) -> List[str]:
    return [_get(r, "id") for r in result_rows(res)]


def result_attrs(res: Any) -> List[Dict[str, Any]]:
    return [_get(r, "attributes") or {} for r in result_rows(res)]


def export_rows(ns: Any) -> list:
    return list(ns.export())


def export_ids(ns: Any) -> List[str]:
    return [_get(r, "id") for r in export_rows(ns)]


def export_attrs_by_id(ns: Any) -> Dict[str, Dict[str, Any]]:
    return {_get(r, "id"): (_get(r, "attributes") or {}) for r in export_rows(ns)}


def eventually(
    check: Callable[[], Any],
    timeout: float = 30.0,
    interval: float = 0.5,
) -> Any:
    """Retry an assertion-raising check until it passes or `timeout` elapses.

    The server guarantees read-your-writes on a single node, but background
    index builds may briefly affect *ranking*-sensitive assertions; this keeps
    the suite robust without papering over real failures.
    """
    deadline = time.monotonic() + timeout
    while True:
        try:
            return check()
        except AssertionError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(interval)
