import pytest

from shoal.batch import iter_document_batches
from shoal.models import Document


def docs(n, prefix="d"):
    return [{"id": f"{prefix}{i}", "attributes": {"i": i}} for i in range(n)]


def test_count_bounded_batching():
    batches = list(iter_document_batches(docs(10), batch_size=4))
    assert [len(b) for b in batches] == [4, 4, 2]
    # All ids preserved, in order.
    flat = [d["id"] for b in batches for d in b]
    assert flat == [f"d{i}" for i in range(10)]


def test_byte_bounded_batching():
    big = [{"id": f"d{i}", "attributes": {"blob": "x" * 1000}} for i in range(6)]
    batches = list(iter_document_batches(big, batch_size=100, max_batch_bytes=2500))
    assert all(len(b) <= 2 for b in batches)
    assert sum(len(b) for b in batches) == 6


def test_oversized_single_document_still_emitted():
    big = [{"id": "huge", "attributes": {"blob": "x" * 10_000}}]
    batches = list(iter_document_batches(big, batch_size=10, max_batch_bytes=100))
    assert len(batches) == 1
    assert batches[0][0]["id"] == "huge"


def test_accepts_document_models():
    models = [Document(id=i, vector=[0.1, 0.2]) for i in range(3)]
    batches = list(iter_document_batches(models, batch_size=2))
    assert [len(b) for b in batches] == [2, 1]
    assert batches[0][0] == {"id": 0, "vector": [0.1, 0.2]}


def test_empty_input_yields_nothing():
    assert list(iter_document_batches([])) == []


def test_invalid_config():
    with pytest.raises(ValueError):
        list(iter_document_batches(docs(1), batch_size=0))
    with pytest.raises(ValueError):
        list(iter_document_batches(docs(1), max_batch_bytes=0))
