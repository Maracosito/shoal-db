"""Pluggable embedding providers for the Lagoon demos and benchmarks.

The database itself is embedding-vendor agnostic: it stores and searches
whatever vectors you give it. These helpers exist only so the demos can
produce vectors. Three providers are available, selected with the
``LAGOON_EMBED_PROVIDER`` environment variable (or the ``--provider`` flag
that every demo script exposes):

  hash     (default) A deterministic local feature-hashing embedder with no
           external dependencies and no network access. It hashes word
           unigrams/bigrams into a fixed-size signed vector. It captures
           lexical overlap, NOT deep semantics — it makes the demos runnable
           offline, but for real semantic quality use one of the providers
           below.

  openai   Calls the OpenAI embeddings REST API directly via ``requests``
           (no SDK dependency). Requires OPENAI_API_KEY. Model defaults to
           ``text-embedding-3-small`` (override with LAGOON_EMBED_MODEL;
           override output dims with LAGOON_EMBED_DIMS for text-embedding-3-*).

  st       Local sentence-transformers models (``all-MiniLM-L6-v2`` by
           default). Requires ``pip install sentence-transformers``; the
           import is lazy so the dependency stays optional.

All providers share the same interface:

    provider = get_provider()           # or get_provider("openai")
    provider.name   -> str
    provider.dims   -> int
    provider.embed(["text a", "text b"]) -> list[list[float]]
"""

from __future__ import annotations

import hashlib
import math
import os
import re
from collections import Counter
from typing import List, Optional, Sequence

_TOKEN_RE = re.compile(r"[a-z0-9]+")


# --------------------------------------------------------------------- hash


class HashingEmbedder:
    """Deterministic signed feature-hashing of word unigrams and bigrams,
    L2-normalised. Zero dependencies, fully offline, stable across runs."""

    name = "hash"

    def __init__(self, dims: int = 256):
        self.dims = dims

    @staticmethod
    def _tokens(text: str) -> List[str]:
        words = _TOKEN_RE.findall(text.lower())
        toks = list(words)
        toks.extend(f"{a}_{b}" for a, b in zip(words, words[1:]))
        return toks

    def _embed_one(self, text: str) -> List[float]:
        vec = [0.0] * self.dims
        for tok, count in Counter(self._tokens(text)).items():
            digest = hashlib.blake2b(tok.encode("utf-8"), digest_size=8).digest()
            idx = int.from_bytes(digest[:4], "little") % self.dims
            sign = 1.0 if digest[4] & 1 else -1.0
            vec[idx] += sign * (1.0 + math.log(count))
        norm = math.sqrt(sum(v * v for v in vec))
        if norm > 0:
            vec = [v / norm for v in vec]
        return vec

    def embed(self, texts: Sequence[str]) -> List[List[float]]:
        return [self._embed_one(t) for t in texts]


# ------------------------------------------------------------------- openai


class OpenAIEmbedder:
    """Calls https://api.openai.com/v1/embeddings via plain HTTP.

    Targets the stable v1 embeddings endpoint (request shape:
    {"model": ..., "input": [...], "dimensions": ...?}; response shape:
    {"data": [{"index": i, "embedding": [...]}]}).
    """

    name = "openai"
    _KNOWN_DIMS = {
        "text-embedding-3-small": 1536,
        "text-embedding-3-large": 3072,
        "text-embedding-ada-002": 1536,
    }

    def __init__(self, model: Optional[str] = None, dims: Optional[int] = None):
        self.api_key = os.environ.get("OPENAI_API_KEY")
        if not self.api_key:
            raise RuntimeError(
                "LAGOON_EMBED_PROVIDER=openai requires the OPENAI_API_KEY "
                "environment variable to be set"
            )
        self.model = model or os.environ.get("LAGOON_EMBED_MODEL") or "text-embedding-3-small"
        env_dims = os.environ.get("LAGOON_EMBED_DIMS")
        self._requested_dims = dims or (int(env_dims) if env_dims else None)
        self.dims = self._requested_dims or self._KNOWN_DIMS.get(self.model, 1536)
        self.base_url = os.environ.get("OPENAI_BASE_URL", "https://api.openai.com/v1").rstrip("/")

    def embed(self, texts: Sequence[str]) -> List[List[float]]:
        import requests  # declared in demos/requirements.txt

        out: List[List[float]] = []
        for start in range(0, len(texts), 128):
            batch = list(texts[start : start + 128])
            body = {"model": self.model, "input": batch}
            # The dimensions parameter is only valid for text-embedding-3-*.
            if self._requested_dims and self.model.startswith("text-embedding-3"):
                body["dimensions"] = self._requested_dims
            resp = requests.post(
                f"{self.base_url}/embeddings",
                headers={"Authorization": f"Bearer {self.api_key}"},
                json=body,
                timeout=120,
            )
            if resp.status_code >= 400:
                raise RuntimeError(f"openai embeddings error {resp.status_code}: {resp.text[:500]}")
            data = sorted(resp.json()["data"], key=lambda d: d["index"])
            out.extend(d["embedding"] for d in data)
        if out:
            self.dims = len(out[0])
        return out


# ------------------------------------------------- sentence-transformers


class SentenceTransformerEmbedder:
    """Local models via the optional ``sentence-transformers`` package."""

    name = "st"

    def __init__(self, model: Optional[str] = None):
        try:
            from sentence_transformers import SentenceTransformer  # lazy, optional
        except ImportError as exc:
            raise RuntimeError(
                "LAGOON_EMBED_PROVIDER=st requires the optional dependency: "
                "pip install sentence-transformers"
            ) from exc
        model_name = model or os.environ.get("LAGOON_EMBED_MODEL") or "all-MiniLM-L6-v2"
        self._model = SentenceTransformer(model_name)
        self.dims = int(self._model.get_sentence_embedding_dimension())

    def embed(self, texts: Sequence[str]) -> List[List[float]]:
        vectors = self._model.encode(list(texts), normalize_embeddings=True)
        return [list(map(float, v)) for v in vectors]


# ---------------------------------------------------------------- factory

_ALIASES = {
    "hash": "hash",
    "local": "hash",
    "openai": "openai",
    "st": "st",
    "sentence-transformers": "st",
    "sbert": "st",
}


def get_provider(name: Optional[str] = None):
    """Return an embedder by name, falling back to LAGOON_EMBED_PROVIDER,
    then to the offline ``hash`` provider."""
    key = (name or os.environ.get("LAGOON_EMBED_PROVIDER") or "hash").lower()
    kind = _ALIASES.get(key)
    if kind == "hash":
        dims = int(os.environ.get("LAGOON_EMBED_DIMS", "256"))
        return HashingEmbedder(dims=dims)
    if kind == "openai":
        return OpenAIEmbedder()
    if kind == "st":
        return SentenceTransformerEmbedder()
    raise ValueError(f"unknown embedding provider {key!r}; choose hash | openai | st")


def embed_texts(texts: Sequence[str], provider=None) -> List[List[float]]:
    """Convenience helper: embed with the given or default provider."""
    return (provider or get_provider()).embed(texts)
