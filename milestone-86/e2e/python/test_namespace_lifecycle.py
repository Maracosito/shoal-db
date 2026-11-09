"""Namespace lifecycle through the Python SDK against a live server."""

from __future__ import annotations

import pytest

from helpers import unique_ns


def _names(client) -> list[str]:
    return [getattr(n, "name", n) for n in client.list_namespaces()]


def test_create_list_delete(client, track):
    name = track(unique_ns("lifecycle"))
    client.create_namespace(name)
    assert name in _names(client)

    client.delete_namespace(name)
    assert name not in _names(client)


def test_namespace_metadata(client, make_namespace):
    name = make_namespace("meta")
    meta = client.get_namespace(name)
    assert meta is not None
    # The metadata payload must identify the namespace it describes.
    rendered = repr(meta)
    assert name in rendered


def test_create_is_visible_immediately(client, make_namespace):
    """Read-your-writes: a created namespace is queryable at once."""
    name = make_namespace("ryw")
    ns = client.namespace(name)
    # An empty namespace exports zero documents rather than erroring.
    assert list(ns.export()) == []


def test_delete_missing_namespace_raises(client):
    from shoal.errors import NotFoundError

    with pytest.raises(NotFoundError):
        client.delete_namespace(unique_ns("missing"))


def test_query_missing_namespace_raises(client):
    from shoal.errors import NotFoundError

    ns = client.namespace(unique_ns("missing"))
    with pytest.raises(NotFoundError):
        ns.query(vector=[1.0, 0.0, 0.0, 0.0], top_k=1)
