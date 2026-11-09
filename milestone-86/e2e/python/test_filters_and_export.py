"""Filter operators and export round-trips through the Python SDK."""

from __future__ import annotations

from helpers import (
    CORPUS,
    NATURE_QUERY,
    corpus_documents,
    eventually,
    export_attrs_by_id,
    export_ids,
    result_ids,
)


def _seed(ns) -> None:
    ns.upsert(corpus_documents())


def _expect(ns, flt, expected_ids: set[str]) -> None:
    def check():
        res = ns.query(filter=flt, top_k=len(CORPUS) + 5)
        assert set(result_ids(res)) == expected_ids

    eventually(check)


def test_filter_eq(namespace):
    from shoal.filters import F

    _seed(namespace)
    _expect(namespace, F.eq("genre", "nature"), {"doc-1", "doc-2"})


def test_filter_numeric_ranges(namespace):
    from shoal.filters import F

    _seed(namespace)
    _expect(namespace, F.gte("year", 2020), {"doc-2", "doc-3", "doc-4", "doc-5"})
    _expect(namespace, F.gt("year", 2021), {"doc-3", "doc-4"})
    _expect(namespace, F.lt("year", 2018), {"doc-7", "doc-8"})
    _expect(namespace, F.lte("year", 2018), {"doc-6", "doc-7", "doc-8"})


def test_filter_in(namespace):
    from shoal.filters import F

    _seed(namespace)
    _expect(
        namespace,
        F.in_("genre", ["nature", "history"]),
        {"doc-1", "doc-2", "doc-7", "doc-8"},
    )


def test_filter_contains_any(namespace):
    from shoal.filters import F

    _seed(namespace)
    _expect(
        namespace,
        F.contains_any("tags", ["ocean", "soup"]),
        {"doc-1", "doc-2", "doc-6", "doc-8"},
    )


def test_filter_and(namespace):
    from shoal.filters import F

    _seed(namespace)
    _expect(namespace, F.eq("genre", "tech") & F.gte("year", 2023), {"doc-3"})


def test_filter_or(namespace):
    from shoal.filters import F

    _seed(namespace)
    _expect(
        namespace,
        F.eq("genre", "history") | F.gte("rating", 4.6),
        {"doc-3", "doc-6", "doc-7", "doc-8"},
    )


def test_filter_not(namespace):
    from shoal.filters import F

    _seed(namespace)
    _expect(
        namespace,
        ~F.eq("genre", "nature"),
        {"doc-3", "doc-4", "doc-5", "doc-6", "doc-7", "doc-8"},
    )


def test_vector_search_with_filter(namespace):
    from shoal.filters import F

    _seed(namespace)

    def check():
        # Among year >= 2020 documents, doc-2 is nearest the nature axis.
        res = namespace.query(
            vector=NATURE_QUERY, filter=F.gte("year", 2020), top_k=2
        )
        ids = result_ids(res)
        assert ids[0] == "doc-2"
        assert "doc-1" not in ids  # excluded by the filter despite proximity

    eventually(check)


def test_export_round_trips_all_documents(namespace):
    _seed(namespace)
    ids = export_ids(namespace)
    assert sorted(ids) == sorted(d["id"] for d in CORPUS)


def test_export_preserves_attribute_types(namespace):
    _seed(namespace)
    attrs = export_attrs_by_id(namespace)["doc-3"]
    assert attrs["title"] == "Vector databases explained"
    assert attrs["year"] == 2023
    assert abs(float(attrs["rating"]) - 4.8) < 1e-9
    assert list(attrs["tags"]) == ["search", "ai"]


def test_export_reflects_deletions(namespace):
    _seed(namespace)
    namespace.delete(ids=["doc-5"])
    assert "doc-5" not in export_ids(namespace)
