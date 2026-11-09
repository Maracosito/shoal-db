"""HTTP client wrapper for the Lagoon search database.

This is a small, dependency-light client used by the bundled demos and the
benchmark suite. For application development prefer the official SDKs in
``clients/python`` and ``clients/typescript``; this module exists so the demos
and benchmarks run from a source checkout with nothing more than ``requests``
installed.

Environment variables:
    LAGOON_URL      Base URL of the API server (default ``http://localhost:8080``)
    LAGOON_API_KEY  API key sent as a Bearer token (default ``lagoon-dev-key``,
                    matching the docker-compose development configuration)

Filter expressions use the documented list form, e.g.::

    ["category", "Eq", "footwear"]
    ["And", [["price", "Lte", 150], ["in_stock", "Eq", True]]]
    ["Or",  [["brand", "In", ["Ridgeline", "Stoneford"]], ["Not", ["lang", "Eq", "markdown"]]]]
"""

from __future__ import annotations

import json
import os
import time
from typing import Any, Dict, Iterable, Iterator, List, Optional, Sequence

import requests

DEFAULT_URL = "http://localhost:8080"
DEFAULT_API_KEY = "lagoon-dev-key"


class LagoonError(RuntimeError):
    """Raised when the Lagoon API returns an error or is unreachable."""

    def __init__(self, status: int, message: str):
        super().__init__(f"lagoon api error (status={status}): {message}")
        self.status = status
        self.message = message


class LagoonClient:
    """Thin synchronous client over the Lagoon v1 HTTP API."""

    def __init__(
        self,
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
        timeout: float = 60.0,
        max_retries: int = 3,
    ):
        self.base_url = (base_url or os.environ.get("LAGOON_URL") or DEFAULT_URL).rstrip("/")
        self.api_key = api_key or os.environ.get("LAGOON_API_KEY") or DEFAULT_API_KEY
        self.timeout = timeout
        self.max_retries = max_retries
        self._session = requests.Session()
        self._session.headers.update(
            {
                "Authorization": f"Bearer {self.api_key}",
                "Content-Type": "application/json",
                "User-Agent": "lagoon-demos/0.1",
            }
        )

    # ------------------------------------------------------------------ core

    def _request(
        self,
        method: str,
        path: str,
        json_body: Optional[dict] = None,
        params: Optional[dict] = None,
        stream: bool = False,
    ) -> requests.Response:
        url = f"{self.base_url}{path}"
        last_exc: Optional[Exception] = None
        for attempt in range(self.max_retries + 1):
            try:
                resp = self._session.request(
                    method,
                    url,
                    json=json_body,
                    params=params,
                    timeout=self.timeout,
                    stream=stream,
                )
            except (requests.ConnectionError, requests.Timeout) as exc:
                last_exc = exc
                if attempt < self.max_retries:
                    time.sleep(min(0.25 * (2 ** attempt), 4.0))
                    continue
                raise LagoonError(0, f"could not reach {url}: {exc}") from exc
            if resp.status_code >= 500 and attempt < self.max_retries:
                time.sleep(min(0.25 * (2 ** attempt), 4.0))
                continue
            if resp.status_code >= 400:
                try:
                    detail = resp.json().get("error", resp.text)
                except ValueError:
                    detail = resp.text
                raise LagoonError(resp.status_code, str(detail))
            return resp
        raise LagoonError(0, f"could not reach {url}: {last_exc}")

    # ---------------------------------------------------------------- health

    def health(self) -> dict:
        return self._request("GET", "/healthz").json()

    def metrics_text(self) -> str:
        """Raw Prometheus exposition text from /metrics."""
        return self._request("GET", "/metrics").text

    # ------------------------------------------------------------ namespaces

    def create_namespace(
        self,
        name: str,
        *,
        dims: Optional[int] = None,
        metric: str = "cosine",
        text_fields: Optional[Sequence[str]] = None,
        schema: Optional[dict] = None,
    ) -> dict:
        """Create (or idempotently re-create) a namespace. PUT is idempotent."""
        body: Dict[str, Any] = {"metric": metric}
        if dims is not None:
            body["dims"] = dims
        if text_fields is not None:
            body["text_fields"] = list(text_fields)
        if schema is not None:
            body["schema"] = schema
        return self._request("PUT", f"/v1/namespaces/{name}", json_body=body).json()

    def get_namespace(self, name: str) -> dict:
        return self._request("GET", f"/v1/namespaces/{name}").json()

    def namespace_exists(self, name: str) -> bool:
        try:
            self.get_namespace(name)
            return True
        except LagoonError as exc:
            if exc.status == 404:
                return False
            raise

    def delete_namespace(self, name: str, *, missing_ok: bool = False) -> None:
        try:
            self._request("DELETE", f"/v1/namespaces/{name}")
        except LagoonError as exc:
            if missing_ok and exc.status == 404:
                return
            raise

    def reset_namespace(self, name: str, **create_kwargs: Any) -> dict:
        """Delete the namespace if it exists, then create it fresh."""
        self.delete_namespace(name, missing_ok=True)
        return self.create_namespace(name, **create_kwargs)

    def list_namespaces(self) -> List[dict]:
        return self._request("GET", "/v1/namespaces").json().get("namespaces", [])

    # ------------------------------------------------------------- documents

    def upsert(
        self,
        namespace: str,
        documents: Sequence[dict],
        *,
        batch_size: int = 200,
    ) -> int:
        """Batched upsert. Each document is a flat dict with an ``id`` key,
        an optional ``vector`` (list of floats) and arbitrary attributes."""
        total = 0
        for start in range(0, len(documents), batch_size):
            batch = list(documents[start : start + batch_size])
            self._request(
                "POST",
                f"/v1/namespaces/{namespace}/documents",
                json_body={"documents": batch},
            )
            total += len(batch)
        return total

    def patch_documents(self, namespace: str, documents: Sequence[dict]) -> dict:
        """Partial update: merges given attributes into existing documents by id."""
        return self._request(
            "POST",
            f"/v1/namespaces/{namespace}/documents/patch",
            json_body={"documents": list(documents)},
        ).json()

    def delete_documents(
        self,
        namespace: str,
        *,
        ids: Optional[Sequence[Any]] = None,
        filter: Optional[list] = None,
    ) -> dict:
        if (ids is None) == (filter is None):
            raise ValueError("pass exactly one of ids= or filter=")
        body: Dict[str, Any] = {}
        if ids is not None:
            body["ids"] = list(ids)
        if filter is not None:
            body["filter"] = filter
        return self._request(
            "POST", f"/v1/namespaces/{namespace}/documents/delete", json_body=body
        ).json()

    # ----------------------------------------------------------------- query

    def query(
        self,
        namespace: str,
        *,
        vector: Optional[Sequence[float]] = None,
        text: Optional[str] = None,
        mode: Optional[str] = None,
        top_k: int = 10,
        filter: Optional[list] = None,
        fusion: Optional[dict] = None,
        fields: Optional[Dict[str, float]] = None,
        include_attributes: Optional[Sequence[str]] = None,
    ) -> List[dict]:
        """Run a single query. Returns a list of hits, each
        ``{"id": ..., "score": float, "attributes": {...}}``.

        mode: "vector" | "bm25" | "hybrid" | None (server infers from inputs)
        fusion: {"method": "rrf", "k": 60}
                or {"method": "weighted", "vector_weight": 0.7, "text_weight": 0.3}
        fields: BM25 field boosts, e.g. {"title": 2.0, "description": 1.0}
        """
        body = self._query_body(
            vector=vector,
            text=text,
            mode=mode,
            top_k=top_k,
            filter=filter,
            fusion=fusion,
            fields=fields,
            include_attributes=include_attributes,
        )
        resp = self._request("POST", f"/v1/namespaces/{namespace}/query", json_body=body)
        return resp.json().get("results", [])

    def multi_query(self, namespace: str, queries: Sequence[dict]) -> List[List[dict]]:
        """Run several queries in one request. Each entry of ``queries`` takes
        the same keyword arguments as :meth:`query` (as a dict)."""
        bodies = [self._query_body(**q) for q in queries]
        resp = self._request(
            "POST",
            f"/v1/namespaces/{namespace}/query",
            json_body={"queries": bodies},
        )
        return [r.get("results", []) for r in resp.json().get("responses", [])]

    @staticmethod
    def _query_body(
        *,
        vector: Optional[Sequence[float]] = None,
        text: Optional[str] = None,
        mode: Optional[str] = None,
        top_k: int = 10,
        filter: Optional[list] = None,
        fusion: Optional[dict] = None,
        fields: Optional[Dict[str, float]] = None,
        include_attributes: Optional[Sequence[str]] = None,
    ) -> dict:
        if vector is None and text is None:
            raise ValueError("query needs vector=, text=, or both")
        body: Dict[str, Any] = {"top_k": top_k}
        if vector is not None:
            body["vector"] = list(vector)
        if text is not None:
            body["text"] = text
        if mode is not None:
            body["mode"] = mode
        if filter is not None:
            body["filter"] = filter
        if fusion is not None:
            body["fusion"] = fusion
        if fields is not None:
            body["fields"] = fields
        if include_attributes is not None:
            body["include_attributes"] = list(include_attributes)
        return body

    # ----------------------------------------------- branching / copy / export

    def branch_namespace(self, source: str, target: str) -> dict:
        """Copy-on-write branch: ``target`` references the source's immutable
        segments; subsequent writes to either side are isolated."""
        return self._request(
            "POST", f"/v1/namespaces/{source}/branch", json_body={"target": target}
        ).json()

    def copy_namespace(self, source: str, target: str) -> dict:
        return self._request(
            "POST", f"/v1/namespaces/{source}/copy", json_body={"target": target}
        ).json()

    def export(self, namespace: str) -> Iterator[dict]:
        """Stream all documents in a namespace as parsed JSON objects."""
        resp = self._request("GET", f"/v1/namespaces/{namespace}/export", stream=True)
        for line in resp.iter_lines():
            if line:
                yield json.loads(line)

    # ----------------------------------------------------------------- cache

    def warm_namespace(self, namespace: str, *, pin: bool = False) -> dict:
        """Preload a namespace's segments into the local disk/memory caches.
        ``pin=True`` keeps it resident across evictions."""
        return self._request(
            "POST", f"/v1/namespaces/{namespace}/warm", json_body={"pin": pin}
        ).json()

    def unpin_namespace(self, namespace: str) -> dict:
        return self._request(
            "POST", f"/v1/namespaces/{namespace}/warm", json_body={"pin": False}
        ).json()


def iter_jsonl(path: str) -> Iterable[dict]:
    """Helper used by the demos to read bundled .jsonl datasets."""
    with open(path, "r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if line:
                yield json.loads(line)
