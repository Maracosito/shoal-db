import httpx
import pytest

from conftest import body_of, fast_retry, make_async_client
from shoal import NotFoundError


async def test_async_namespace_lifecycle_and_query():
    seen = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append((request.method, request.url.path))
        if request.url.path == "/v1/namespaces" and request.method == "POST":
            return httpx.Response(200, json={"name": "docs", "dimensions": 2})
        if request.url.path == "/v1/namespaces/docs/documents":
            return httpx.Response(200, json={"upserted": 2, "sequence": 1})
        if request.url.path == "/v1/namespaces/docs/query":
            assert body_of(request)["mode"] == "vector"
            return httpx.Response(
                200, json={"matches": [{"id": "x", "score": 0.9}], "took_ms": 1.0}
            )
        return httpx.Response(404, json={"error": {"message": "not found"}})

    async with make_async_client(handler) as client:
        info = await client.create_namespace("docs", dimensions=2)
        assert info.name == "docs"

        ns = client.namespace("docs")
        upserted = await ns.upsert(
            documents=[{"id": "x", "vector": [1.0, 0.0]}, {"id": "y", "vector": [0.0, 1.0]}]
        )
        assert upserted.upserted == 2

        result = await ns.query(vector=[1.0, 0.0], top_k=1)
        assert result.matches[0].id == "x"

    assert ("POST", "/v1/namespaces") in seen


async def test_async_export_pagination():
    pages = [
        {"documents": [{"id": 1}, {"id": 2}], "next_cursor": "next"},
        {"documents": [{"id": 3}], "next_cursor": None},
    ]
    calls = {"n": 0}

    def handler(request: httpx.Request) -> httpx.Response:
        page = pages[calls["n"]]
        calls["n"] += 1
        return httpx.Response(200, json=page)

    async with make_async_client(handler) as client:
        ids = [doc.id async for doc in client.namespace("ns").export(batch_size=2)]
    assert ids == [1, 2, 3]


async def test_async_upsert_many_with_async_callback():
    def handler(request: httpx.Request) -> httpx.Response:
        n = len(body_of(request)["documents"])
        return httpx.Response(200, json={"upserted": n})

    progress = []

    async def on_batch(index, response):
        progress.append((index, response.upserted))

    async with make_async_client(handler) as client:
        result = await client.namespace("ns").upsert_many(
            [{"id": i} for i in range(5)], batch_size=2, on_batch=on_batch
        )
    assert result.total_upserted == 5
    assert result.batches == 3
    assert progress == [(0, 2), (1, 2), (2, 1)]


async def test_async_retries_then_succeeds():
    attempts = {"n": 0}

    def handler(request: httpx.Request) -> httpx.Response:
        attempts["n"] += 1
        if attempts["n"] == 1:
            return httpx.Response(502, json={})
        return httpx.Response(200, json={"status": "ok"})

    async with make_async_client(handler, retry=fast_retry(max_retries=2)) as client:
        health = await client.health()
    assert health.status == "ok"
    assert attempts["n"] == 2


async def test_async_error_mapping():
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(404, json={"error": {"code": "nope", "message": "gone"}})

    async with make_async_client(handler) as client:
        with pytest.raises(NotFoundError) as excinfo:
            await client.namespace("missing").info()
    assert excinfo.value.code == "nope"
