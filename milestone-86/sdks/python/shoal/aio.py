"""Asynchronous Shoal client (httpx.AsyncClient based)."""

from __future__ import annotations

import os
from typing import (
    Any,
    AsyncIterator,
    Awaitable,
    Callable,
    Dict,
    Iterable,
    List,
    Mapping,
    Optional,
    Sequence,
    Union,
)

import httpx

from . import _base
from ._transport import RetryConfig, send_with_retries_async
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


class AsyncShoal:
    """Asynchronous client for the Shoal HTTP API.

    Example::

        from shoal import AsyncShoal

        async with AsyncShoal(api_key="sk-...") as client:
            ns = client.namespace("articles")
            results = await ns.query(text="object storage", top_k=5)
    """

    def __init__(
        self,
        api_key: Optional[str] = None,
        *,
        base_url: Optional[str] = None,
        timeout: float = 30.0,
        retry: Optional[RetryConfig] = None,
        http_client: Optional[httpx.AsyncClient] = None,
        extra_headers: Optional[Mapping[str, str]] = None,
    ) -> None:
        self._api_key = api_key if api_key is not None else os.environ.get("SHOAL_API_KEY")
        self._base_url = (
            base_url or os.environ.get("SHOAL_BASE_URL") or _base.DEFAULT_BASE_URL
        ).rstrip("/")
        self._retry = retry or RetryConfig()
        self._owns_client = http_client is None
        self._http = http_client or httpx.AsyncClient(timeout=timeout)
        self._headers: Dict[str, str] = {
            "User-Agent": f"shoal-python/{__version__}",
            "Accept": "application/json",
        }
        if self._api_key:
            self._headers["Authorization"] = f"Bearer {self._api_key}"
        if extra_headers:
            self._headers.update(extra_headers)

    # -- lifecycle ---------------------------------------------------------

    async def aclose(self) -> None:
        if self._owns_client:
            await self._http.aclose()

    async def __aenter__(self) -> "AsyncShoal":
        return self

    async def __aexit__(self, *exc_info: Any) -> None:
        await self.aclose()

    # -- plumbing ----------------------------------------------------------

    async def _request(
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
        response = await send_with_retries_async(self._http, request, self._retry)
        raise_for_response(response)
        if response.status_code == 204 or not response.content:
            return {}
        data = response.json()
        return data if isinstance(data, dict) else {"data": data}

    # -- server-level operations --------------------------------------------

    async def health(self) -> HealthResponse:
        return HealthResponse.model_validate(await self._request("GET", "/v1/health"))

    async def list_namespaces(self) -> List[NamespaceInfo]:
        data = await self._request("GET", _base.API_PREFIX + "/namespaces")
        return [NamespaceInfo.model_validate(ns) for ns in data.get("namespaces", [])]

    async def create_namespace(
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
        data = await self._request("POST", _base.API_PREFIX + "/namespaces", json_body=body)
        return NamespaceInfo.model_validate(data)

    async def delete_namespace(self, name: str) -> None:
        await self._request("DELETE", _base.ns_path(name))

    def namespace(self, name: str) -> "AsyncNamespace":
        return AsyncNamespace(self, name)


class AsyncNamespace:
    """Async handle for operations on one namespace."""

    def __init__(self, client: AsyncShoal, name: str) -> None:
        self._client = client
        self.name = name

    def __repr__(self) -> str:
        return f"AsyncNamespace({self.name!r})"

    # -- metadata ------------------------------------------------------------

    async def info(self) -> NamespaceInfo:
        data = await self._client._request("GET", _base.ns_path(self.name))
        return NamespaceInfo.model_validate(data)

    async def update_metadata(self, metadata: Mapping[str, Any]) -> NamespaceInfo:
        data = await self._client._request(
            "PATCH", _base.ns_path(self.name), json_body={"metadata": dict(metadata)}
        )
        return NamespaceInfo.model_validate(data)

    async def delete(self) -> None:
        await self._client.delete_namespace(self.name)

    # -- writes ----------------------------------------------------------------

    async def upsert(
        self,
        documents: Optional[Iterable[_base.DocumentLike]] = None,
        *,
        ids: Optional[Sequence[DocumentId]] = None,
        vectors: Optional[Sequence[Sequence[float]]] = None,
        attributes: Optional[Mapping[str, Sequence[Any]]] = None,
        idempotency_key: Optional[str] = None,
    ) -> UpsertResponse:
        body = _base.build_upsert_body(
            documents, ids=ids, vectors=vectors, attributes=attributes
        )
        headers = {"Idempotency-Key": idempotency_key} if idempotency_key else None
        data = await self._client._request(
            "POST", _base.ns_path(self.name, "documents"), json_body=body, headers=headers
        )
        return UpsertResponse.model_validate(data)

    async def upsert_many(
        self,
        documents: Iterable[_base.DocumentLike],
        *,
        batch_size: int = DEFAULT_BATCH_SIZE,
        max_batch_bytes: int = DEFAULT_MAX_BATCH_BYTES,
        on_batch: Optional[Callable[[int, UpsertResponse], Optional[Awaitable[None]]]] = None,
    ) -> UpsertManyResult:
        """Upsert a large iterable of documents in batches. `on_batch` may be
        a plain function or a coroutine function."""
        import inspect

        total = 0
        batches = 0
        for batch in iter_document_batches(
            documents, batch_size=batch_size, max_batch_bytes=max_batch_bytes
        ):
            data = await self._client._request(
                "POST",
                _base.ns_path(self.name, "documents"),
                json_body={"documents": batch},
            )
            response = UpsertResponse.model_validate(data)
            total += response.upserted
            if on_batch is not None:
                result = on_batch(batches, response)
                if inspect.isawaitable(result):
                    await result
            batches += 1
        return UpsertManyResult(total_upserted=total, batches=batches)

    async def patch(
        self,
        documents: Iterable[_base.DocumentLike],
        *,
        idempotency_key: Optional[str] = None,
    ) -> PatchResponse:
        body = _base.build_patch_body(documents)
        headers = {"Idempotency-Key": idempotency_key} if idempotency_key else None
        data = await self._client._request(
            "PATCH", _base.ns_path(self.name, "documents"), json_body=body, headers=headers
        )
        return PatchResponse.model_validate(data)

    async def delete_documents(
        self,
        *,
        ids: Optional[Sequence[DocumentId]] = None,
        filter: Optional[FilterLike] = None,
        idempotency_key: Optional[str] = None,
    ) -> DeleteResponse:
        body = _base.build_delete_body(ids=ids, filter=filter)
        headers = {"Idempotency-Key": idempotency_key} if idempotency_key else None
        data = await self._client._request(
            "POST", _base.ns_path(self.name, "documents", "delete"), json_body=body, headers=headers
        )
        return DeleteResponse.model_validate(data)

    # -- queries ---------------------------------------------------------------

    async def query(
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
        data = await self._client._request(
            "POST", _base.ns_path(self.name, "query"), json_body=body
        )
        return QueryResponse.model_validate(data)

    async def multi_query(
        self, queries: Sequence[Union[Query, Mapping[str, Any]]]
    ) -> List[QueryResponse]:
        body = _base.build_multi_query_body(queries)
        data = await self._client._request(
            "POST", _base.ns_path(self.name, "query"), json_body=body
        )
        return [QueryResponse.model_validate(r) for r in data.get("results", [])]

    # -- export ------------------------------------------------------------------

    async def export(self, *, batch_size: int = 500) -> AsyncIterator[Document]:
        """Async-iterate every document in the namespace."""
        cursor: Optional[str] = None
        while True:
            params: Dict[str, Any] = {"limit": batch_size}
            if cursor:
                params["cursor"] = cursor
            data = await self._client._request(
                "GET", _base.ns_path(self.name, "export"), params=params
            )
            page = ExportPage.model_validate(data)
            for doc in page.documents:
                yield doc
            if not page.next_cursor or not page.documents:
                return
            cursor = page.next_cursor

    # -- cache & lifecycle ops -----------------------------------------------------

    async def warm(self, *, segments: Optional[Sequence[str]] = None) -> WarmResponse:
        body: Dict[str, Any] = {}
        if segments is not None:
            body["segments"] = list(segments)
        data = await self._client._request(
            "POST", _base.ns_path(self.name, "warm"), json_body=body
        )
        return WarmResponse.model_validate(data)

    async def pin(self) -> None:
        await self._client._request(
            "POST", _base.ns_path(self.name, "pin"), json_body={"pinned": True}
        )

    async def unpin(self) -> None:
        await self._client._request(
            "POST", _base.ns_path(self.name, "pin"), json_body={"pinned": False}
        )

    async def branch(self, target: str) -> "AsyncNamespace":
        await self._client._request(
            "POST", _base.ns_path(self.name, "branch"), json_body={"target": target}
        )
        return AsyncNamespace(self._client, target)

    async def copy(self, target: str) -> "AsyncNamespace":
        await self._client._request(
            "POST", _base.ns_path(self.name, "copy"), json_body={"target": target}
        )
        return AsyncNamespace(self._client, target)
