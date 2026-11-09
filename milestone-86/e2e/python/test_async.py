"""Async client round-trip against a live server."""

from __future__ import annotations

from helpers import (
    E2E_API_KEY,
    NATURE_QUERY,
    corpus_documents,
    corpus_ids,
    result_ids,
    unique_ns,
)


async def test_async_full_roundtrip(base_url, track):
    from shoal.aio import AsyncClient
    from shoal.filters import F

    name = track(unique_ns("aio"))
    async with AsyncClient(base_url=base_url, api_key=E2E_API_KEY) as client:
        await client.create_namespace(name)
        ns = client.namespace(name)

        await ns.upsert(corpus_documents())

        res = await ns.query(vector=NATURE_QUERY, top_k=3)
        ids = result_ids(res)
        assert ids[0] == "doc-1"

        res = await ns.query(text="coral reef fish ocean", top_k=3)
        assert "doc-1" in result_ids(res)

        res = await ns.query(filter=F.eq("genre", "tech"), top_k=10)
        assert set(result_ids(res)) == {"doc-3", "doc-4"}

        await ns.delete(ids=["doc-1"])
        exported = [getattr(d, "id", d.get("id") if isinstance(d, dict) else None)
                    async for d in ns.export()] if hasattr(ns.export(), "__aiter__") else None
        if exported is None:
            exported = [getattr(d, "id", d["id"] if isinstance(d, dict) else None)
                        for d in await _collect(ns)]
        assert "doc-1" not in exported
        assert len(exported) == len(corpus_ids()) - 1

        await client.delete_namespace(name)


async def _collect(ns):
    out = await ns.export()
    return list(out)
