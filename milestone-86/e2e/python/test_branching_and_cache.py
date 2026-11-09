"""Copy-on-write branching, namespace copies, and cache warm/pin."""

from __future__ import annotations

from helpers import (
    NATURE_QUERY,
    corpus_documents,
    corpus_ids,
    eventually,
    export_ids,
    result_ids,
    unique_ns,
)


def _seed(ns) -> None:
    ns.upsert(corpus_documents())


def _extra_doc(doc_id: str):
    from shoal import Document

    return Document(
        id=doc_id,
        vector=[0.5, 0.5, 0.0, 0.0],
        attributes={"title": doc_id, "genre": "extra"},
    )


def test_branch_sees_source_data(client, make_namespace, track):
    src_name = make_namespace("src")
    src = client.namespace(src_name)
    _seed(src)

    branch_name = track(unique_ns("branch"))
    client.branch_namespace(src_name, branch_name)
    branch = client.namespace(branch_name)

    assert sorted(export_ids(branch)) == sorted(corpus_ids())

    def check():
        res = branch.query(vector=NATURE_QUERY, top_k=1)
        assert result_ids(res)[0] == "doc-1"

    eventually(check)


def test_branch_writes_do_not_leak_to_source(client, make_namespace, track):
    src_name = make_namespace("src")
    src = client.namespace(src_name)
    _seed(src)

    branch_name = track(unique_ns("branch"))
    client.branch_namespace(src_name, branch_name)
    branch = client.namespace(branch_name)

    branch.upsert([_extra_doc("branch-only")])
    assert "branch-only" in export_ids(branch)
    assert "branch-only" not in export_ids(src)


def test_source_writes_do_not_leak_to_branch(client, make_namespace, track):
    src_name = make_namespace("src")
    src = client.namespace(src_name)
    _seed(src)

    branch_name = track(unique_ns("branch"))
    client.branch_namespace(src_name, branch_name)
    branch = client.namespace(branch_name)

    src.upsert([_extra_doc("src-only")])
    assert "src-only" in export_ids(src)
    assert "src-only" not in export_ids(branch)


def test_multi_level_branches_are_isolated(client, make_namespace, track):
    src_name = make_namespace("src")
    src = client.namespace(src_name)
    _seed(src)

    b1_name = track(unique_ns("b1"))
    client.branch_namespace(src_name, b1_name)
    b1 = client.namespace(b1_name)
    b1.upsert([_extra_doc("level-1")])

    b2_name = track(unique_ns("b2"))
    client.branch_namespace(b1_name, b2_name)
    b2 = client.namespace(b2_name)
    b2.upsert([_extra_doc("level-2")])

    src_ids = set(export_ids(src))
    b1_ids = set(export_ids(b1))
    b2_ids = set(export_ids(b2))

    assert "level-1" not in src_ids and "level-2" not in src_ids
    assert "level-1" in b1_ids and "level-2" not in b1_ids
    assert "level-1" in b2_ids and "level-2" in b2_ids


def test_deleting_branch_preserves_source(client, make_namespace, track):
    src_name = make_namespace("src")
    src = client.namespace(src_name)
    _seed(src)

    branch_name = unique_ns("branch")
    client.branch_namespace(src_name, branch_name)
    client.delete_namespace(branch_name)

    # Source data is fully intact and still queryable after the branch
    # (which referenced shared immutable segments) is gone.
    assert sorted(export_ids(src)) == sorted(corpus_ids())

    def check():
        res = src.query(vector=NATURE_QUERY, top_k=1)
        assert result_ids(res)[0] == "doc-1"

    eventually(check)


def test_copy_namespace_is_independent(client, make_namespace, track):
    src_name = make_namespace("src")
    src = client.namespace(src_name)
    _seed(src)

    copy_name = track(unique_ns("copy"))
    client.copy_namespace(src_name, copy_name)
    copy = client.namespace(copy_name)

    assert sorted(export_ids(copy)) == sorted(corpus_ids())

    copy.delete(ids=["doc-1"])
    assert "doc-1" not in export_ids(copy)
    assert "doc-1" in export_ids(src)


def test_warm_and_pin(client, make_namespace):
    name = make_namespace("warm")
    ns = client.namespace(name)
    _seed(ns)

    # Warming must succeed and queries afterwards behave identically.
    ns.warm()

    def check():
        res = ns.query(vector=NATURE_QUERY, top_k=1)
        assert result_ids(res)[0] == "doc-1"

    eventually(check)

    # Pin / unpin round-trips without error.
    ns.pin()
    ns.unpin()
