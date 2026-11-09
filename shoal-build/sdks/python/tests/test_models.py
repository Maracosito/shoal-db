import pytest
from pydantic import ValidationError

from shoal import Document, Query, QueryResponse, SparseVector


def test_document_to_wire_omits_unset_fields():
    doc = Document(id="a1")
    assert doc.to_wire() == {"id": "a1"}

    full = Document(
        id=7,
        vector=[0.1, 0.2],
        sparse_vector=SparseVector(indices=[3, 9], values=[0.5, 0.25]),
        attributes={"lang": "en"},
    )
    assert full.to_wire() == {
        "id": 7,
        "vector": [0.1, 0.2],
        "sparse_vector": {"indices": [3, 9], "values": [0.5, 0.25]},
        "attributes": {"lang": "en"},
    }


def test_sparse_vector_length_mismatch_rejected():
    with pytest.raises(ValidationError):
        SparseVector(indices=[1, 2], values=[0.5])


def test_query_response_helpers():
    response = QueryResponse.model_validate(
        {"matches": [{"id": "a", "score": 1.0}, {"id": "b", "score": 0.5}], "took_ms": 2.0}
    )
    assert response.ids() == ["a", "b"]
    assert len(response) == 2
    assert [m.id for m in response] == ["a", "b"]


def test_query_to_wire_infers_mode():
    assert Query(text="hello").to_wire()["mode"] == "text"
    assert Query(vector=[0.1]).to_wire()["mode"] == "vector"
    wire = Query(vector=[0.1], text="hello").to_wire()
    assert wire["mode"] == "hybrid"
    assert wire["fusion"]["method"] == "rrf"


def test_query_to_wire_validation():
    with pytest.raises(ValueError):
        Query().to_wire()
    with pytest.raises(ValueError):
        Query(text="x", mode="vector").to_wire()
    with pytest.raises(ValueError):
        Query(text="x", top_k=0).to_wire()
