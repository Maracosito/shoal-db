"""Shared, transport-agnostic request building used by the sync and async
clients. This module contains no I/O so it is easy to test in isolation."""

from __future__ import annotations

from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence, Union
from urllib.parse import quote

from .filters import Filter
from .models import Document, Query, SparseVector

DEFAULT_BASE_URL = "http://localhost:8780"
API_PREFIX = "/v1"
DEFAULT_ORG = "default"
DEFAULT_PROJECT = "default"

DocumentLike = Union[Document, Mapping[str, Any]]


def ns_prefix(org: str = DEFAULT_ORG, project: str = DEFAULT_PROJECT) -> str:
    """Build the /v1/orgs/{org}/projects/{project}/namespaces path prefix."""
    return f"{API_PREFIX}/orgs/{quote(org, safe='')}/projects/{quote(project, safe='')}/namespaces"


def ns_path(namespace: str, *suffix: str, org: str = DEFAULT_ORG, project: str = DEFAULT_PROJECT) -> str:
    """Build a /v1/orgs/{org}/projects/{project}/namespaces/{ns}[/suffix...] path."""
    parts = [ns_prefix(org, project), quote(namespace, safe="")]
    parts.extend(suffix)
    return "/".join(p.strip("/") if i else p for i, p in enumerate(parts))


def normalize_filter(flt: Optional[Union[Filter, Mapping[str, Any]]]) -> Optional[Dict[str, Any]]:
    if flt is None:
        return None
    if isinstance(flt, Filter):
        return flt.to_dict()
    if isinstance(flt, Mapping):
        return dict(flt)
    raise TypeError(
        f"filter must be a shoal.filters.Filter or a dict, got {type(flt).__name__}"
    )


def document_to_wire(doc: DocumentLike) -> Dict[str, Any]:
    if isinstance(doc, Document):
        return doc.to_wire()
    if isinstance(doc, Mapping):
        if "id" not in doc:
            raise ValueError("document dict is missing required key 'id'")
        payload: Dict[str, Any] = {}
        for key, value in doc.items():
            if value is None:
                continue
            if key == "sparse_vector" and isinstance(value, SparseVector):
                payload[key] = value.model_dump()
            else:
                payload[key] = value
        return payload
    raise TypeError(
        f"document must be a shoal.Document or a dict, got {type(doc).__name__}"
    )


def build_upsert_body(
    documents: Optional[Iterable[DocumentLike]] = None,
    *,
    ids: Optional[Sequence[Any]] = None,
    vectors: Optional[Sequence[Sequence[float]]] = None,
    attributes: Optional[Mapping[str, Sequence[Any]]] = None,
) -> Dict[str, Any]:
    """Build a row-oriented or column-oriented upsert body.

    Exactly one of `documents` (row-oriented) or `ids` (column-oriented,
    with optional parallel `vectors` and `attributes` columns) must be given.
    """
    row = documents is not None
    col = ids is not None
    if row == col:
        raise ValueError("provide exactly one of `documents` (rows) or `ids` (columns)")

    if row:
        assert documents is not None
        wire = [document_to_wire(d) for d in documents]
        if not wire:
            raise ValueError("`documents` must contain at least one document")
        return {"documents": wire}

    assert ids is not None
    ids_list = list(ids)
    if not ids_list:
        raise ValueError("`ids` must contain at least one id")
    columns: Dict[str, Any] = {"ids": ids_list}
    if vectors is not None:
        vectors_list = [list(v) for v in vectors]
        if len(vectors_list) != len(ids_list):
            raise ValueError(
                f"`vectors` length ({len(vectors_list)}) must match `ids` length ({len(ids_list)})"
            )
        columns["vectors"] = vectors_list
    if attributes is not None:
        attr_columns: Dict[str, List[Any]] = {}
        for field, values in attributes.items():
            values_list = list(values)
            if len(values_list) != len(ids_list):
                raise ValueError(
                    f"attribute column '{field}' length ({len(values_list)}) "
                    f"must match `ids` length ({len(ids_list)})"
                )
            attr_columns[field] = values_list
        columns["attributes"] = attr_columns
    return {"columns": columns}


def build_patch_body(documents: Iterable[DocumentLike]) -> Dict[str, Any]:
    wire = [document_to_wire(d) for d in documents]
    if not wire:
        raise ValueError("`documents` must contain at least one patch")
    return {"documents": wire}


def build_delete_body(
    ids: Optional[Sequence[Any]] = None,
    filter: Optional[Union[Filter, Mapping[str, Any]]] = None,
) -> Dict[str, Any]:
    if (ids is None) == (filter is None):
        raise ValueError("provide exactly one of `ids` or `filter`")
    if ids is not None:
        ids_list = list(ids)
        if not ids_list:
            raise ValueError("`ids` must contain at least one id")
        return {"ids": ids_list}
    return {"filter": normalize_filter(filter)}


def build_query_body(
    vector: Optional[Sequence[float]] = None,
    text: Optional[str] = None,
    *,
    mode: str = "auto",
    top_k: int = 10,
    filter: Optional[Union[Filter, Mapping[str, Any]]] = None,
    include_attributes: Optional[Sequence[str]] = None,
    include_vectors: bool = False,
    fusion: str = "rrf",
    vector_weight: float = 0.5,
    text_weight: float = 0.5,
    rrf_k: int = 60,
    text_fields: Optional[Mapping[str, float]] = None,
    consistency: Optional[str] = None,
) -> Dict[str, Any]:
    if vector is None and text is None:
        raise ValueError("a query requires `vector`, `text`, or both")
    if top_k < 1:
        raise ValueError("`top_k` must be >= 1")

    if mode == "auto":
        if vector is not None and text is not None:
            mode = "hybrid"
        elif vector is not None:
            mode = "vector"
        else:
            mode = "text"
    if mode in ("vector", "hybrid") and vector is None:
        raise ValueError(f"mode '{mode}' requires a `vector`")
    if mode in ("text", "hybrid") and text is None:
        raise ValueError(f"mode '{mode}' requires `text`")

    body: Dict[str, Any] = {"top_k": top_k}
    if vector is not None:
        body["vector"] = list(vector)
    if text is not None:
        text_obj: Dict[str, Any] = {"query": text}
        if text_fields:
            text_obj["fields"] = dict(text_fields)
        body["text"] = text_obj
    flt = normalize_filter(filter)
    if flt is not None:
        body["filter"] = flt
    if include_attributes is not None:
        body["include_attributes"] = list(include_attributes)
    if include_vectors:
        body["include_vector"] = True
    if consistency is not None:
        body["consistency"] = consistency
    if mode == "hybrid":
        if fusion == "rrf":
            body["fusion"] = {"method": "rrf", "rrf_k": rrf_k}
        elif fusion == "weighted":
            body["fusion"] = {
                "method": "weighted",
                "vector_weight": vector_weight,
                "text_weight": text_weight,
            }
        else:
            raise ValueError(f"unknown fusion method '{fusion}' (expected 'rrf' or 'weighted')")
    return body


def build_multi_query_body(queries: Sequence[Union[Query, Mapping[str, Any]]]) -> Dict[str, Any]:
    if not queries:
        raise ValueError("`queries` must contain at least one query")
    wire: List[Dict[str, Any]] = []
    for q in queries:
        if isinstance(q, Query):
            wire.append(q.to_wire())
        elif isinstance(q, Mapping):
            wire.append(dict(q))
        else:
            raise TypeError(f"query must be a shoal.Query or a dict, got {type(q).__name__}")
    return {"queries": wire}


def build_create_namespace_body(
    name: str,
    *,
    dimensions: Optional[int] = None,
    distance_metric: Optional[str] = None,
    metadata: Optional[Mapping[str, Any]] = None,
) -> Dict[str, Any]:
    if not name:
        raise ValueError("namespace name must be non-empty")
    body: Dict[str, Any] = {"name": name}
    if dimensions is not None:
        body["dimensions"] = dimensions
    if distance_metric is not None:
        body["distance_metric"] = distance_metric
    if metadata is not None:
        body["metadata"] = dict(metadata)
    return body
