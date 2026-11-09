"""Shoal Python SDK — official client for the Shoal search database.

Quickstart::

    from shoal import Shoal, Field

    client = Shoal(api_key="sk-...")
    ns = client.namespace("articles")
    ns.upsert(documents=[
        {"id": "a1", "vector": [0.1, 0.2, 0.3],
         "attributes": {"title": "Hello", "lang": "en"}},
    ])
    results = ns.query(
        vector=[0.1, 0.2, 0.3],
        filter=Field("lang") == "en",
        top_k=5,
    )
"""

from . import filters
from ._transport import RetryConfig
from ._version import __version__
from .aio import AsyncNamespace, AsyncShoal
from .client import Namespace, Shoal
from .errors import (
    APIError,
    AuthenticationError,
    AuthorizationError,
    BadRequestError,
    ConflictError,
    NotFoundError,
    RateLimitError,
    ServerError,
    ShoalError,
    TransportError,
)
from .filters import Field, Filter
from .models import (
    DeleteResponse,
    Document,
    HealthResponse,
    NamespaceInfo,
    PatchResponse,
    Query,
    QueryMatch,
    QueryResponse,
    SparseVector,
    UpsertManyResult,
    UpsertResponse,
    WarmResponse,
)

__all__ = [
    "__version__",
    # clients
    "Shoal",
    "Namespace",
    "AsyncShoal",
    "AsyncNamespace",
    "RetryConfig",
    # filters
    "filters",
    "Filter",
    "Field",
    # models
    "Document",
    "SparseVector",
    "NamespaceInfo",
    "Query",
    "QueryMatch",
    "QueryResponse",
    "UpsertResponse",
    "UpsertManyResult",
    "PatchResponse",
    "DeleteResponse",
    "WarmResponse",
    "HealthResponse",
    # errors
    "ShoalError",
    "TransportError",
    "APIError",
    "BadRequestError",
    "AuthenticationError",
    "AuthorizationError",
    "NotFoundError",
    "ConflictError",
    "RateLimitError",
    "ServerError",
]
