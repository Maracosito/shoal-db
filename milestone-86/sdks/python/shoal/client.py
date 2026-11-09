"""Synchronous Shoal client."""

from __future__ import annotations

import os
from typing import Any, Callable, Dict, Iterable, Iterator, List, Mapping, Optional, Sequence, Union

import httpx

from . import _base
from ._transport import RetryConfig, send_with_retries
from ._version import __version__
from .batch import DEFAULT_BATCH_SIZE, DEFAULT_MAX_BATCH_BYTES, iter_document_batches
from .errors import raise_for_response
from .filters import Filter
from .models import (
    DeleteResponse,
    Document,
    DocumentId,
    ExportPage,
    HealthResponse,
    NamespaceInfo,
    PatchResponse,
    Query,
    QueryResponse,
    UpsertManyResult,
    UpsertResponse,
    WarmResponse,
)

FilterLike = Union[Filter, Mapping[str, Any]]


class Shoal:
    """Synchronous client for the Shoal HTTP API.

    Example::

        from shoal import Shoal

        client = Shoal(api_key="sk-...", base_url="http://localhost:8780")
        ns = client.namespace("articles")
        ns.upsert(documents=[{"id": "a1", "vector": [...], "attributes": {"lang": "en"}}])
        results = ns.query(vector=[...], top_k=5)
    """

    def __init__(
        self,
        api_key: Optional[str] = None,
        *,
        base_url: Optional[str] = None,
        timeout: float = 30.0,
        retry: Optional[RetryConfig] = None,
        http_client: Optional[httpx.Client] = None,
        extra_headers: Optional[Mapping[str, str]] = None,
    ) -> None:
        self._api_key = api_key if api_key is not None else os.environ.get("SHOAL_API_KEY")
        self._base_url = (
            base_url or os.environ.get("SHOAL_BASE_URL") or _base.DEFAULT_BASE_URL
        ).rstrip("/")
        self._retry = retry or RetryConfig()
        self._owns_client = http_client is None
        self._http = http_client or httpx.Client(timeout=timeout)
        self._headers: Dict[str, str] = {
            "User-Agent": f"shoal-python/{__version__}",
            "Accept": "application/json",
        }
        if self._api_key:
            self._headers["Authorization"] = f"Bearer {self._api_key}"
        if extra_headers:
            self._headers.update(extra_headers)

    # -- lifecycle ---------------------------------------------------------

    def close(self) -> None:
        if self._owns_client:
            self._http.close()

    def __enter__(self) -> "Shoal":
        return self

    def __exit__(self, *exc_info: Any) -> None:
        self.close()

    # -- plumbing ----------------------------------------------------------

    def _request(
        self,
        method: str,
        path: str,
        *,
        json_body: Optional[Dict[str, Any]] = None,
        params: Optional[Mapping[str, Any]] = None,
        headers: Optional[Mapping[str, str]] = None,
    ) -> Dict[str, Any]:
        url = self._base_url + path
        merged_headers = dict(self._headers)
        if headers:
            merged_headers.update(headers)
        request = self._http.build_request(
            method, url, json=json_body, params=params, headers=merged_headers
        )
        response = send_with_retries(self._http, request, self._retry)
        raise_for_response(response)
        if response.status_code == 204 or not response.content:
            return {}
        data = response.json()
        return data if isinstance(data, dict) else {"data": data}

    # -- server-level operations --------------------------------------------

    def health(self) -> HealthResponse:
        """Check API server health (no authentication required)."""
        return HealthResponse.model_validate(self._request("GET", "/v1/health"))

    def list_namespaces(self) -> List[NamespaceInfo]:
        data = self._request("GET", _base.API_PREFIX + "/namespaces")
        return [NamespaceInfo.model_validate(ns) for ns in data.get("namespaces", [])]

    def create_namespace(
        self,
        name: str,
        *,
        dimensions: Optional[int] = None,
        distance_metric: Optional[str] = None,
        metadata: Optional[Mapping[str, Any]] = None,
    ) -> NamespaceInfo:
        body = _base.build_create_namespace_body(
            name,
            dimensions=dimensions,
            distance_metric=distance_metric,
            metadata=metadata,
        )
        data = self._request("POST", _base.API_PREFIX + "/namespaces", json_body=body)
        return NamespaceInfo.model_validate(data)

    def delete_namespace(self, name: str) -> None:
        self._request("DELETE", _base.ns_path(name))

    def namespace(self, name: str) -> "Namespace":
        """Get a handle to a namespace (does not call the server)."""
        return Namespace(self, name)


class Namespace:
    """Handle for operations on one namespace. Obtain via `Shoal.namespace`."""

    def __init__(self, client: Shoal, name: str) -> None:
        self._client = client
        self.name = name

    def __repr__(self) -> str:
        return f"Namespace({self.name!r})"

    # -- metadata ------------------------------------------------------------

    def info(self) -> NamespaceInfo:
        return NamespaceInfo.model_validate(self._client._request("GET", _base.ns_path(self.name)))

    def update_metadata(self, metadata: Mapping[str, Any]) -> NamespaceInfo:
        data = self._client._request(
            "PATCH", _base.ns_path(self.name), json_body={"metadata": dict(metadata)}
        )
        return NamespaceInfo.model_validate(data)

    def delete(self) -> None:
        """Delete this namespace and all its documents."""
        self._client.delete_namespace(self.name)

    # -- writes ----------------------------------------------------------------

    def upsert(
        self,
        documents: Optional[Iterable[_base.DocumentLike]] = None,
        *,
        ids: Optional[Sequence[DocumentId]] = None,
        vectors: Optional[Sequence[Sequence[float]]] = None,
        attributes: Optional[Mapping[str, Sequence[Any]]] = None,
        idempotency_key: Optional[str] = None,
    ) -> UpsertResponse:
        """Upsert documents, row-oriented (`documents`) or column-oriented
        (`ids` + optional `vectors` / `attributes` columns)."""
        body = _base.build_upsert_body(
            documents, ids=ids, vectors=vectors, attributes=attributes
        )
        headers = {"Idempotency-Key": idempotency_key} if idempotency_key else None
        data = self._client._request(
            "POST", _base.ns_path(self.name, "documents"), json_body=body, headers=headers
        )
        return UpsertResponse.model_validate(data)

    def upsert_many(
        self,
        documents: Iterable[_base.DocumentLike],
        *,
        batch_size: int = DEFAULT_BATCH_SIZE,
        max_batch_bytes: int = DEFAULT_MAX_BATCH_BYTES,
        on_batch: Optional[Callable[[int, UpsertResponse], None]] = None,
    ) -> UpsertManyResult:
        """Upsert an arbitrarily large iterable of documents in batches.

        `on_batch(index, response)` is invoked after each successful batch,
        which is useful for progress reporting.
        """
        total = 0
        batches = 0
        for batch in iter_document_batches(
            documents, batch_size=batch_size, max_batch_bytes=max_batch_bytes
        ):
            data = self._client._request(
                "POST",
                _base.ns_path(self.name, "documents"),
                json_body={"documents": batch},
            )
            response = UpsertResponse.model_validate(data)
            total += response.upserted
            if on_batch is not None:
                on_batch(batches, response)
            batches += 1
        return UpsertManyResult(total_upserted=total, batches=batches)

    def patch(
        self,
        documents: Iterable[_base.DocumentLike],
        *,
        idempotency_key: Optional[str] = None,
    ) -> PatchResponse:
        """Partially update documents: provided attributes are merged into
        existing documents; vectors are replaced if provided."""
        body = _base.build_patch_body(documents)
        headers = {"Idempotency-Key": idempotency_key} if idempotency_key else None
        data = self._client._request(
            "PATCH", _base.ns_path(self.name, "documents"), json_body=body, headers=headers
        )
        return PatchResponse.model_validate(data)

    def delete_documents(
        self,
        *,
        ids: Optional[Sequence[DocumentId]] = None,
        filter: Optional[FilterLike] = None,
        idempotency_key: Optional[str] = None,
    ) -> DeleteResponse:
        """Delete documents by id list or by filter (exactly one required)."""
        body = _base.build_delete_body(ids=ids, filter=filter)
        headers = {"Idempotency-Key": idempotency_key} if idempotency_key else None
        data = self._client._request(
            "POST", _base.ns_path(self.name, "documents", "delete"), json_body=body, headers=headers
        )
        return DeleteResponse.model_validate(data)

    # -- queries ---------------------------------------------------------------

    def query(
        self,
        vector: Optional[Sequence[float]] = None,
        text: Optional[str] = None,
        *,
        mode: str = "auto",
        top_k: int = 10,
        filter: Optional[FilterLike] = None,
        include_attributes: Optional[Sequence[str]] = None,
        include_vectors: bool = False,
        fusion: str = "rrf",
        vector_weight: float = 0.5,
        text_weight: float = 0.5,
        rrf_k: int = 60,
        text_fields: Optional[Mapping[str, float]] = None,
        consistency: Optional[str] = None,
    ) -> QueryResponse:
        """Run a vector, full-text, or hybrid query.

        With both `vector` and `text` supplied (and mode "auto"), results are
        fused via reciprocal rank fusion by default; pass `fusion="weighted"`
        with `vector_weight` / `text_weight` for weighted score fusion.
        """
        body = _base.build_query_body(
            vector=vector,
            text=text,
            mode=mode,
            top_k=top_k,
            filter=filter,
            include_attributes=include_attributes,
            include_vectors=include_vectors,
            fusion=fusion,
            vector_weight=vector_weight,
            text_weight=text_weight,
            rrf_k=rrf_k,
            text_fields=text_fields,
            consistency=consistency,
        )
        data = self._client._request("POST", _base.ns_path(self.name, "query"), json_body=body)
        return QueryResponse.model_validate(data)

    def multi_query(
        self, queries: Sequence[Union[Query, Mapping[str, Any]]]
    ) -> List[QueryResponse]:
        """Execute several queries in one round-trip."""
        body = _base.build_multi_query_body(queries)
        data = self._client._request("POST", _base.ns_path(self.name, "query"), json_body=body)
        return [QueryResponse.model_validate(r) for r in data.get("results", [])]

    # -- export ------------------------------------------------------------------

    def export(self, *, batch_size: int = 500) -> Iterator[Document]:
        """Stream every document in the namespace via cursor pagination."""
        cursor: Optional[str] = None
        while True:
            params: Dict[str, Any] = {"limit": batch_size}
            if cursor:
                params["cursor"] = cursor
            data = self._client._request("GET", _base.ns_path(self.name, "export"), params=params)
            page = ExportPage.model_validate(data)
            for doc in page.documents:
                yield doc
            if not page.next_cursor or not page.documents:
                return
            cursor = page.next_cursor

    # -- cache & lifecycle ops -----------------------------------------------------

    def warm(self, *, segments: Optional[Sequence[str]] = None) -> WarmResponse:
        """Preload this namespace (or selected segments) into local caches."""
        body: Dict[str, Any] = {}
        if segments is not None:
            body["segments"] = list(segments)
        data = self._client._request("POST", _base.ns_path(self.name, "warm"), json_body=body)
        return WarmResponse.model_validate(data)

    def pin(self) -> None:
        """Keep this namespace resident in the cache (exempt from eviction)."""
        self._client._request("POST", _base.ns_path(self.name, "pin"), json_body={"pinned": True})

    def unpin(self) -> None:
        self._client._request("POST", _base.ns_path(self.name, "pin"), json_body={"pinned": False})

    def branch(self, target: str) -> "Namespace":
        """Create a copy-on-write branch of this namespace named `target`."""
        self._client._request(
            "POST", _base.ns_path(self.name, "branch"), json_body={"target": target}
        )
        return Namespace(self._client, target)

    def copy(self, target: str) -> "Namespace":
        """Create a full, independent copy of this namespace named `target`."""
        self._client._request(
            "POST", _base.ns_path(self.name, "copy"), json_body={"target": target}
        )
        return Namespace(self._client, target)
