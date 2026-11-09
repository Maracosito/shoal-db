"""Typed request/response models for the Shoal Python SDK (Pydantic v2)."""

from __future__ import annotations

from typing import Any, Dict, List, Optional, Sequence, Union

from pydantic import BaseModel, ConfigDict, Field, model_validator

try:
    from typing import Literal
except ImportError:  # pragma: no cover - Python < 3.8 fallback, unused
    from typing_extensions import Literal  # type: ignore

DocumentId = Union[str, int]
DistanceMetric = Literal["cosine", "dot", "euclidean"]
QueryMode = Literal["auto", "vector", "text", "hybrid"]
FusionMethod = Literal["rrf", "weighted"]
Consistency = Literal["strong", "eventual"]


class SparseVector(BaseModel):
    """A sparse vector encoded as parallel index/value arrays."""

    indices: List[int]
    values: List[float]

    @model_validator(mode="after")
    def _check_lengths(self) -> "SparseVector":
        if len(self.indices) != len(self.values):
            raise ValueError(
                f"sparse vector indices ({len(self.indices)}) and values "
                f"({len(self.values)}) must have equal length"
            )
        return self


class Document(BaseModel):
    """A document stored in a Shoal namespace."""

    model_config = ConfigDict(populate_by_name=True)

    id: DocumentId
    vector: Optional[List[float]] = None
    sparse_vector: Optional[SparseVector] = None
    attributes: Dict[str, Any] = Field(default_factory=dict)

    def to_wire(self) -> Dict[str, Any]:
        payload: Dict[str, Any] = {"id": self.id}
        if self.vector is not None:
            payload["vector"] = list(self.vector)
        if self.sparse_vector is not None:
            payload["sparse_vector"] = self.sparse_vector.model_dump()
        if self.attributes:
            payload["attributes"] = self.attributes
        return payload


class NamespaceInfo(BaseModel):
    """Metadata describing a namespace."""

    model_config = ConfigDict(extra="allow")

    name: str
    dimensions: Optional[int] = None
    distance_metric: Optional[DistanceMetric] = None
    approx_doc_count: Optional[int] = None
    parent: Optional[str] = None
    pinned: bool = False
    created_at: Optional[str] = None
    metadata: Dict[str, Any] = Field(default_factory=dict)


class QueryMatch(BaseModel):
    """A single result row from a query."""

    model_config = ConfigDict(extra="allow")

    id: DocumentId
    score: float
    attributes: Dict[str, Any] = Field(default_factory=dict)
    vector: Optional[List[float]] = None


class QueryResponse(BaseModel):
    """The result of one query execution."""

    matches: List[QueryMatch] = Field(default_factory=list)
    took_ms: Optional[float] = None
    plan: Optional[str] = None

    def ids(self) -> List[DocumentId]:
        return [m.id for m in self.matches]

    def __len__(self) -> int:
        return len(self.matches)

    def __iter__(self) -> Any:  # iterate over matches directly
        return iter(self.matches)


class Query(BaseModel):
    """A declarative query, primarily used with `multi_query`."""

    model_config = ConfigDict(arbitrary_types_allowed=True)

    vector: Optional[Sequence[float]] = None
    text: Optional[str] = None
    mode: QueryMode = "auto"
    top_k: int = 10
    filter: Optional[Any] = None  # shoal.filters.Filter or a wire-format dict
    include_attributes: Optional[List[str]] = None
    include_vectors: bool = False
    fusion: FusionMethod = "rrf"
    vector_weight: float = 0.5
    text_weight: float = 0.5
    rrf_k: int = 60
    text_fields: Optional[Dict[str, float]] = None
    consistency: Optional[Consistency] = None

    def to_wire(self) -> Dict[str, Any]:
        from ._base import build_query_body

        return build_query_body(
            vector=self.vector,
            text=self.text,
            mode=self.mode,
            top_k=self.top_k,
            filter=self.filter,
            include_attributes=self.include_attributes,
            include_vectors=self.include_vectors,
            fusion=self.fusion,
            vector_weight=self.vector_weight,
            text_weight=self.text_weight,
            rrf_k=self.rrf_k,
            text_fields=self.text_fields,
            consistency=self.consistency,
        )


class UpsertResponse(BaseModel):
    upserted: int = 0
    sequence: Optional[int] = None


class PatchResponse(BaseModel):
    patched: int = 0
    sequence: Optional[int] = None


class DeleteResponse(BaseModel):
    deleted: int = 0
    sequence: Optional[int] = None


class WarmResponse(BaseModel):
    status: str = "ok"
    segments_warmed: Optional[int] = None


class HealthResponse(BaseModel):
    model_config = ConfigDict(extra="allow")

    status: str
    version: Optional[str] = None


class ExportPage(BaseModel):
    documents: List[Document] = Field(default_factory=list)
    next_cursor: Optional[str] = None


class UpsertManyResult(BaseModel):
    """Aggregate result of a batched upsert."""

    total_upserted: int = 0
    batches: int = 0
