"""Ingest + query behaviour through the Python SDK against a live server."""

from __future__ import annotations

from helpers import (
    CORPUS,
    NATURE_QUERY,
    TECH_QUERY,
    corpus_documents,
    corpus_ids,
    eventually,
    export_attrs_by_id,
    export_ids,
    result_attrs,
    result_ids,
)


def _seed(ns) -> None:
    ns.upsert(corpus_documents())


# ---------------------------------------------------------------------------
# Query behaviour
# ---------------------------------------------------------------------------

def test_vector_search_returns_nearest_first(namespace):
    _seed(namespace)

    def check():
        res = namespace.query(vector=NATURE_QUERY, top_k=3)
        ids = result_ids(res)
        assert len(ids) == 3
        assert ids[0] == "doc-1"
        # doc-2 is the second-nearest to the nature axis.
        assert ids[1] == "doc-2"

    eventually(check)


def test_fulltext_search_ranks_lexical_matches(namespace):
    _seed(namespace)

    def check():
        res = namespace.query(text="coral reef fish ocean", top_k=5)
        ids = result_ids(res)
        assert ids, "full-text query returned no results"
        assert ids[0] == "doc-1"

    eventually(check)


def test_hybrid_search_combines_signals(namespace):
    _seed(namespace)

    def check():
        res = namespace.query(
            vector=TECH_QUERY,
            text="inverted index ranking",
            top_k=4,
        )
        ids = result_ids(res)
        # Both tech documents are strong on at least one signal each.
        assert "doc-3" in ids
        assert "doc-4" in ids
        assert ids[0] in ("doc-3", "doc-4")

    eventually(check)


def test_hybrid_search_rrf_fusion(namespace):
    _seed(namespace)

    def check():
        res = namespace.query(
            vector=TECH_QUERY,
            text="inverted index ranking",
            top_k=4,
            fusion="rrf",
        )
        ids = result_ids(res)
        assert "doc-3" in ids and "doc-4" in ids

    eventually(check)


def test_top_k_is_respected(namespace):
    _seed(namespace)
    for k in (1, 2, 5):
        res = namespace.query(vector=NATURE_QUERY, top_k=k)
        assert len(result_ids(res)) == k


def test_include_attributes_projects_fields(namespace):
    _seed(namespace)

    def check():
        res = namespace.query(
            vector=NATURE_QUERY, top_k=1, include_attributes=["title"]
        )
        attrs = result_attrs(res)[0]
        assert "title" in attrs
        assert "body" not in attrs

    eventually(check)


# ---------------------------------------------------------------------------
# Mutations
# ---------------------------------------------------------------------------

def test_upsert_is_idempotent(namespace):
    _seed(namespace)
    _seed(namespace)  # re-upserting the identical batch must not duplicate
    ids = export_ids(namespace)
    assert sorted(ids) == sorted(corpus_ids())


def test_upsert_overwrites_existing_document(namespace):
    from shoal import Document

    _seed(namespace)
    namespace.upsert(
        [
            Document(
                id="doc-1",
                vector=CORPUS[0]["vector"],
                attributes={**CORPUS[0]["attributes"], "rating": 1.0},
            )
        ]
    )
    attrs = export_attrs_by_id(namespace)["doc-1"]
    assert attrs["rating"] == 1.0


def test_patch_updates_attributes_in_place(namespace):
    _seed(namespace)
    namespace.patch([{"id": "doc-1", "attributes": {"genre": "updated"}}])

    by_id = export_attrs_by_id(namespace)
    assert by_id["doc-1"]["genre"] == "updated"
    # Patch must not clobber other attributes.
    assert by_id["doc-1"]["title"] == "Coral reef ecosystems"
    # Other documents are untouched.
    assert by_id["doc-2"]["genre"] == "nature"


def test_delete_by_id(namespace):
    _seed(namespace)
    namespace.delete(ids=["doc-1", "doc-2"])
    ids = export_ids(namespace)
    assert "doc-1" not in ids
    assert "doc-2" not in ids
    assert len(ids) == len(CORPUS) - 2


def test_delete_by_filter(namespace):
    from shoal.filters import F

    _seed(namespace)
    namespace.delete_by_filter(F.eq("genre", "history"))
    ids = set(export_ids(namespace))
    assert ids == {"doc-1", "doc-2", "doc-3", "doc-4", "doc-5", "doc-6"}


def test_deleted_documents_do_not_appear_in_results(namespace):
    _seed(namespace)
    namespace.delete(ids=["doc-1"])

    def check():
        res = namespace.query(vector=NATURE_QUERY, top_k=len(CORPUS))
        assert "doc-1" not in result_ids(res)

    eventually(check)
