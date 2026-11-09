"""Batching helpers for high-throughput ingestion.

Splits a stream of documents into batches bounded by both document count and
approximate serialized size, so very large documents do not blow past the
server's request size limits and very small documents still get good
throughput.
"""

from __future__ import annotations

import json
from typing import Any, Dict, Iterable, Iterator, List

from ._base import DocumentLike, document_to_wire

DEFAULT_BATCH_SIZE = 256
DEFAULT_MAX_BATCH_BYTES = 8 * 1024 * 1024  # 8 MiB


def _approx_size(payload: Dict[str, Any]) -> int:
    return len(json.dumps(payload, separators=(",", ":"), default=str))


def iter_document_batches(
    documents: Iterable[DocumentLike],
    *,
    batch_size: int = DEFAULT_BATCH_SIZE,
    max_batch_bytes: int = DEFAULT_MAX_BATCH_BYTES,
) -> Iterator[List[Dict[str, Any]]]:
    """Yield wire-format document batches bounded by count and byte size.

    A document larger than `max_batch_bytes` on its own is emitted as a
    single-document batch rather than dropped.
    """
    if batch_size < 1:
        raise ValueError("`batch_size` must be >= 1")
    if max_batch_bytes < 1:
        raise ValueError("`max_batch_bytes` must be >= 1")

    batch: List[Dict[str, Any]] = []
    batch_bytes = 2  # surrounding "[]"
    for doc in documents:
        payload = document_to_wire(doc)
        size = _approx_size(payload) + 1  # trailing comma
        if batch and (len(batch) >= batch_size or batch_bytes + size > max_batch_bytes):
            yield batch
            batch = []
            batch_bytes = 2
        batch.append(payload)
        batch_bytes += size
    if batch:
        yield batch
