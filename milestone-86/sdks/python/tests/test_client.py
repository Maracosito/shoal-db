import json

import httpx
import pytest

from conftest import body_of, fast_retry, make_client
from shoal import (
    Document,
    Field,
    NotFoundError,
    Query,
    RateLimitError,
    RetryConfig,
    ServerError,
    TransportError,
)
import shoal._transport as transport_module


# ---------------------------------------------------------------------------
# Namespace lifecycle
# ---------------------------------------------------------------------------

def test_create_namespace_sends_expected_body():
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["method"] = request.method
        seen["path"] = request.url.path
        seen["auth"] = request.headers.get("Authorization")
        seen["body"] = body_of(request)
        return httpx.Response(
            200,
            json={"name": "articles", "dimensions": 3, "distance_metric": "cosine"},
        )

    client = make_client(handler)
    info = client.create_namespace("articles", dimensions=3, distance_metric="cosine")
    assert seen["method"] == "POST"
    assert seen["path"] == "/v1/namespaces"
    assert seen["auth"] == "Bearer test-key"
    assert seen["body"] == {"name": "articles", "dimensions": 3, "distance_metric": "cosine"}
    assert info.name == "articles"
    assert info.dimensions == 3


def test_namespace_name_is_url_escaped():
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["path"] = request.url.path
        return httpx.Response(200, json={"name": "team/dev"})

    client = make_client(handler)
    client.namespace("team/dev").info()
    assert seen["path"] == "/v1/namespaces/team%2Fdev"


def test_list_namespaces():
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            200, json={"namespaces": [{"name": "a"}, {"name": "b", "pinned": True}]}
        )

    client = make_client(handler)
    namespaces = client.list_namespaces()
    assert [ns.name for ns in namespaces] == ["a", "b"]
    assert namespaces[1].pinned is True


# ---------------------------------------------------------------------------
# Writes
# ---------------------------------------------------------------------------

def test_upsert_rows_mixed_documents_and_dicts():
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["path"] = request.url.path
        seen["body"] = body_of(request)
        return httpx.Response(200, json={"upserted": 2, "sequence": 7})

    client = make_client(handler)
    ns = client.namespace("articles")
    response = ns.upsert(
        documents=[
            Document(id="a1", vector=[0.1, 0.2], attributes={"lang": "en"}),
            {"id": "a2", "attributes": {"lang": "fr"}},
        ]
    )
    assert seen["path"] == "/v1/namespaces/articles/documents"
    assert seen["body"] == {
        "documents": [
            {"id": "a1", "vector": [0.1, 0.2], "attributes": {"lang": "en"}},
            {"id": "a2", "attributes": {"lang": "fr"}},
        ]
    }
    assert response.upserted == 2
    assert response.sequence == 7


def test_upsert_columns():
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["body"] = body_of(request)
        return httpx.Response(200, json={"upserted": 2})

    client = make_client(handler)
    client.namespace("ns").upsert(
        ids=["a", "b"],
        vectors=[[1.0, 0.0], [0.0, 1.0]],
        attributes={"lang": ["en", "fr"]},
    )
    assert seen["body"] == {
        "columns": {
            "ids": ["a", "b"],
            "vectors": [[1.0, 0.0], [0.0, 1.0]],
            "attributes": {"lang": ["en", "fr"]},
        }
    }


def test_upsert_rejects_both_rows_and_columns():
    client = make_client(lambda request: httpx.Response(200, json={}))
    with pytest.raises(ValueError):
        client.namespace("ns").upsert(documents=[{"id": 1}], ids=[1])
    with pytest.raises(ValueError):
        client.namespace("ns").upsert()


def test_upsert_idempotency_key_header():
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["key"] = request.headers.get("Idempotency-Key")
        return httpx.Response(200, json={"upserted": 1})

    client = make_client(handler)
    client.namespace("ns").upsert(documents=[{"id": 1}], idempotency_key="abc-123")
    assert seen["key"] == "abc-123"


def test_upsert_many_batches_and_progress():
    calls = []

    def handler(request: httpx.Request) -> httpx.Response:
        batch = body_of(request)["documents"]
        calls.append(len(batch))
        return httpx.Response(200, json={"upserted": len(batch)})

    client = make_client(handler)
    progress = []
    result = client.namespace("ns").upsert_many(
        ({"id": i} for i in range(10)),
        batch_size=4,
        on_batch=lambda i, r: progress.append((i, r.upserted)),
    )
    assert calls == [4, 4, 2]
    assert result.total_upserted == 10
    assert result.batches == 3
    assert progress == [(0, 4), (1, 4), (2, 2)]


def test_patch_documents():
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["method"] = request.method
        seen["body"] = body_of(request)
        return httpx.Response(200, json={"patched": 1})

    client = make_client(handler)
    response = client.namespace("ns").patch([{"id": "a1", "attributes": {"stars": 5}}])
    assert seen["method"] == "PATCH"
    assert seen["body"] == {"documents": [{"id": "a1", "attributes": {"stars": 5}}]}
    assert response.patched == 1


def test_delete_by_ids_and_by_filter():
    seen = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append((request.url.path, body_of(request)))
        return httpx.Response(200, json={"deleted": 1})

    client = make_client(handler)
    ns = client.namespace("ns")
    ns.delete_documents(ids=["a", "b"])
    ns.delete_documents(filter=Field("lang") == "fr")
    assert seen[0] == ("/v1/namespaces/ns/documents/delete", {"ids": ["a", "b"]})
    assert seen[1] == (
        "/v1/namespaces/ns/documents/delete",
        {"filter": {"field": "lang", "op": "eq", "value": "fr"}},
    )
    with pytest.raises(ValueError):
        ns.delete_documents()
    with pytest.raises(ValueError):
        ns.delete_documents(ids=["a"], filter=Field("x") == 1)


# ---------------------------------------------------------------------------
# Queries
# ---------------------------------------------------------------------------

def test_vector_query():
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["body"] = body_of(request)
        return httpx.Response(
            200,
            json={
                "matches": [
                    {"id": "a1", "score": 0.91, "attributes": {"title": "Hello"}},
                    {"id": "a2", "score": 0.85},
                ],
                "took_ms": 4.2,
            },
        )

    client = make_client(handler)
    response = client.namespace("ns").query(
        vector=[0.1, 0.2],
        top_k=2,
        filter=Field("lang") == "en",
        include_attributes=["title"],
    )
    assert seen["body"] == {
        "mode": "vector",
        "top_k": 2,
        "vector": [0.1, 0.2],
        "filter": {"field": "lang", "op": "eq", "value": "en"},
        "include_attributes": ["title"],
    }
    assert response.ids() == ["a1", "a2"]
    assert response.matches[0].attributes["title"] == "Hello"
    assert response.took_ms == 4.2


def test_hybrid_query_default_rrf_fusion():
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["body"] = body_of(request)
        return httpx.Response(200, json={"matches": []})

    client = make_client(handler)
    client.namespace("ns").query(vector=[0.1], text="hello world", top_k=5)
    assert seen["body"]["mode"] == "hybrid"
    assert seen["body"]["fusion"] == {"method": "rrf", "rrf_k": 60}


def test_hybrid_query_weighted_fusion():
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["body"] = body_of(request)
        return httpx.Response(200, json={"matches": []})

    client = make_client(handler)
    client.namespace("ns").query(
        vector=[0.1],
        text="hello",
        fusion="weighted",
        vector_weight=0.7,
        text_weight=0.3,
    )
    assert seen["body"]["fusion"] == {
        "method": "weighted", "vector_weight": 0.7, "text_weight": 0.3,
    }


def test_query_requires_vector_or_text():
    client = make_client(lambda request: httpx.Response(200, json={}))
    with pytest.raises(ValueError):
        client.namespace("ns").query()


def test_multi_query():
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["body"] = body_of(request)
        return httpx.Response(
            200,
            json={
                "results": [
                    {"matches": [{"id": 1, "score": 1.0}]},
                    {"matches": [{"id": 2, "score": 0.5}]},
                ]
            },
        )

    client = make_client(handler)
    responses = client.namespace("ns").multi_query(
        [Query(text="alpha", top_k=3), {"mode": "text", "text": "beta", "top_k": 1}]
    )
    assert seen["body"]["queries"][0]["text"] == "alpha"
    assert seen["body"]["queries"][0]["mode"] == "text"
    assert seen["body"]["queries"][0]["top_k"] == 3
    assert seen["body"]["queries"][1]["text"] == "beta"
    assert len(responses) == 2
    assert responses[0].matches[0].id == 1


# ---------------------------------------------------------------------------
# Export, warm, pin, branch, copy
# ---------------------------------------------------------------------------

def test_export_paginates_with_cursor():
    pages = [
        {"documents": [{"id": "a"}, {"id": "b"}], "next_cursor": "tok-1"},
        {"documents": [{"id": "c"}], "next_cursor": None},
    ]
    cursors = []

    def handler(request: httpx.Request) -> httpx.Response:
        cursors.append(request.url.params.get("cursor"))
        return httpx.Response(200, json=pages[len(cursors) - 1])

    client = make_client(handler)
    docs = list(client.namespace("ns").export(batch_size=2))
    assert [d.id for d in docs] == ["a", "b", "c"]
    assert cursors == [None, "tok-1"]


def test_warm_pin_branch_copy():
    seen = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append((request.url.path, body_of(request) if request.content else {}))
        return httpx.Response(200, json={"status": "ok"})

    client = make_client(handler)
    ns = client.namespace("main")
    ns.warm()
    ns.pin()
    ns.unpin()
    branch = ns.branch("feature-x")
    copy = ns.copy("backup")
    assert seen[0][0] == "/v1/namespaces/main/warm"
    assert seen[1] == ("/v1/namespaces/main/pin", {"pinned": True})
    assert seen[2] == ("/v1/namespaces/main/pin", {"pinned": False})
    assert seen[3] == ("/v1/namespaces/main/branch", {"target": "feature-x"})
    assert seen[4] == ("/v1/namespaces/main/copy", {"target": "backup"})
    assert branch.name == "feature-x"
    assert copy.name == "backup"


# ---------------------------------------------------------------------------
# Errors and retries
# ---------------------------------------------------------------------------

def test_404_raises_not_found_with_code():
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            404,
            json={"error": {"code": "namespace_not_found", "message": "no such namespace"}},
        )

    client = make_client(handler)
    with pytest.raises(NotFoundError) as excinfo:
        client.namespace("missing").info()
    assert excinfo.value.status_code == 404
    assert excinfo.value.code == "namespace_not_found"
    assert "no such namespace" in str(excinfo.value)


def test_retries_503_then_succeeds():
    attempts = {"n": 0}

    def handler(request: httpx.Request) -> httpx.Response:
        attempts["n"] += 1
        if attempts["n"] < 3:
            return httpx.Response(503, json={"error": {"message": "overloaded"}})
        return httpx.Response(200, json={"status": "ok"})

    client = make_client(handler, retry=fast_retry(max_retries=3))
    assert client.health().status == "ok"
    assert attempts["n"] == 3


def test_retries_exhausted_raises_server_error():
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(503, json={"error": {"message": "down"}})

    client = make_client(handler, retry=fast_retry(max_retries=1))
    with pytest.raises(ServerError):
        client.health()


def test_429_honors_retry_after(monkeypatch):
    sleeps = []
    monkeypatch.setattr(transport_module.time, "sleep", lambda s: sleeps.append(s))
    attempts = {"n": 0}

    def handler(request: httpx.Request) -> httpx.Response:
        attempts["n"] += 1
        if attempts["n"] == 1:
            return httpx.Response(429, headers={"Retry-After": "3"}, json={})
        return httpx.Response(200, json={"status": "ok"})

    client = make_client(
        handler,
        retry=RetryConfig(max_retries=2, initial_backoff=0.5, jitter=False),
    )
    assert client.health().status == "ok"
    assert sleeps == [3.0]


def test_429_without_retries_raises_rate_limit():
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(429, headers={"Retry-After": "7"}, json={})

    client = make_client(handler, retry=fast_retry(max_retries=0))
    with pytest.raises(RateLimitError) as excinfo:
        client.health()
    assert excinfo.value.retry_after == 7.0


def test_connection_error_retried_then_raises_transport_error():
    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("connection refused", request=request)

    client = make_client(handler, retry=fast_retry(max_retries=1))
    with pytest.raises(TransportError):
        client.health()


def test_no_auth_header_when_api_key_missing(monkeypatch):
    monkeypatch.delenv("SHOAL_API_KEY", raising=False)
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["auth"] = request.headers.get("Authorization")
        return httpx.Response(200, json={"status": "ok"})

    client = make_client(handler, api_key=None)
    client.health()
    assert seen["auth"] is None
