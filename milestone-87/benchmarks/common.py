"""Shared utilities for the Lagoon benchmark suite.

This module is intentionally self-contained (stdlib + requests + numpy) so each
benchmark script can be run on its own.  It provides:

  * ``LagoonBenchClient`` — a thin HTTP client over the Lagoon JSON API.
  * Latency measurement helpers (percentiles, summaries).
  * A ``BenchReport`` container that serialises results to JSON together with
    enough environment metadata to make the run reproducible and honest.

All timing uses ``time.perf_counter_ns`` (monotonic, high resolution) and
measures *client-observed* latency, i.e. including HTTP and JSON overhead.
That is deliberate: it is the latency an application actually experiences.
Server-internal timings, when the server returns them (``took_ms``), are
recorded separately so the two can be compared.
"""

from __future__ import annotations

import json
import os
import platform
import socket
import statistics
import time
from dataclasses import dataclass, field
from typing import Any, Dict, Iterable, List, Optional, Sequence

import requests

DEFAULT_BASE_URL = os.environ.get("LAGOON_URL", "http://localhost:8484")
DEFAULT_API_KEY = os.environ.get("LAGOON_API_KEY", "dev-key")


# --------------------------------------------------------------------------
# HTTP client
# --------------------------------------------------------------------------


class LagoonBenchClient:
    """Minimal Lagoon API client used by the benchmark scripts.

    Endpoint paths follow the v1 HTTP API (see ``docs/api-reference.md`` and
    the OpenAPI spec in ``api/openapi.yaml``).  If you have changed routes in
    a fork, adjust them here in one place.
    """

    def __init__(
        self,
        base_url: str = DEFAULT_BASE_URL,
        api_key: str = DEFAULT_API_KEY,
        timeout: float = 120.0,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout
        self.session = requests.Session()
        self.session.headers.update(
            {
                "Authorization": f"Bearer {api_key}",
                "Content-Type": "application/json",
            }
        )

    # -- low level ---------------------------------------------------------

    def _url(self, path: str) -> str:
        return f"{self.base_url}{path}"

    def request(self, method: str, path: str, body: Optional[dict] = None) -> dict:
        resp = self.session.request(
            method,
            self._url(path),
            data=json.dumps(body) if body is not None else None,
            timeout=self.timeout,
        )
        if resp.status_code >= 400:
            raise RuntimeError(
                f"{method} {path} -> {resp.status_code}: {resp.text[:500]}"
            )
        if resp.content:
            return resp.json()
        return {}

    # -- namespace lifecycle -------------------------------------------------

    def create_namespace(self, name: str, **options: Any) -> dict:
        body = {"name": name}
        body.update(options)
        return self.request("POST", "/v1/namespaces", body)

    def delete_namespace(self, name: str) -> None:
        try:
            self.request("DELETE", f"/v1/namespaces/{name}")
        except RuntimeError as exc:
            if "404" not in str(exc):
                raise

    def namespace_info(self, name: str) -> dict:
        return self.request("GET", f"/v1/namespaces/{name}")

    def warm_namespace(self, name: str) -> dict:
        return self.request("POST", f"/v1/namespaces/{name}/warm", {})

    # -- writes --------------------------------------------------------------

    def upsert(self, namespace: str, documents: List[dict]) -> dict:
        return self.request(
            "POST", f"/v1/namespaces/{namespace}/documents", {"documents": documents}
        )

    # -- queries ---------------------------------------------------------------

    def query(self, namespace: str, query: dict) -> dict:
        return self.request("POST", f"/v1/namespaces/{namespace}/query", query)

    # -- metrics ----------------------------------------------------------------

    def metrics_text(self) -> str:
        resp = self.session.get(self._url("/metrics"), timeout=self.timeout)
        resp.raise_for_status()
        return resp.text

    def wait_for_indexing(self, namespace: str, timeout_s: float = 300.0) -> bool:
        """Poll namespace info until background indexing reports no pending
        WAL entries, or the timeout elapses.  Returns True if indexing caught
        up.  Falls back gracefully if the server build does not expose the
        ``indexing`` block."""
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            info = self.namespace_info(namespace)
            idx = info.get("indexing") or {}
            pending = idx.get("pending_wal_entries")
            if pending is None:
                # Server build without indexing-lag reporting; assume done
                # after a short grace period.
                time.sleep(2.0)
                return True
            if pending == 0:
                return True
            time.sleep(0.5)
        return False


# --------------------------------------------------------------------------
# Query builders (single source of truth for payload shape)
# --------------------------------------------------------------------------


def vector_query(
    vector: Sequence[float],
    top_k: int = 10,
    metric: str = "cosine",
    mode: str = "auto",
    filter_expr: Optional[dict] = None,
    include_attributes: Optional[List[str]] = None,
) -> dict:
    q: dict = {
        "top_k": top_k,
        "vector": {"values": list(vector), "metric": metric, "mode": mode},
    }
    if filter_expr is not None:
        q["filter"] = filter_expr
    if include_attributes is not None:
        q["include_attributes"] = include_attributes
    return q


def text_query(
    query: str,
    fields: Optional[List[str]] = None,
    top_k: int = 10,
    filter_expr: Optional[dict] = None,
) -> dict:
    q: dict = {"top_k": top_k, "text": {"query": query}}
    if fields:
        q["text"]["fields"] = fields
    if filter_expr is not None:
        q["filter"] = filter_expr
    return q


def hybrid_query(
    vector: Sequence[float],
    query: str,
    top_k: int = 10,
    fusion: str = "rrf",
    vector_weight: float = 0.5,
    text_weight: float = 0.5,
    metric: str = "cosine",
    mode: str = "auto",
) -> dict:
    q: dict = {
        "top_k": top_k,
        "vector": {"values": list(vector), "metric": metric, "mode": mode},
        "text": {"query": query},
        "fusion": {"method": fusion},
    }
    if fusion == "weighted":
        q["fusion"]["weights"] = {"vector": vector_weight, "text": text_weight}
    return q


# --------------------------------------------------------------------------
# Timing helpers
# --------------------------------------------------------------------------


def percentile(sorted_values: Sequence[float], p: float) -> float:
    """Linear-interpolation percentile over an already sorted sequence."""
    if not sorted_values:
        return float("nan")
    if len(sorted_values) == 1:
        return sorted_values[0]
    rank = (p / 100.0) * (len(sorted_values) - 1)
    lo = int(rank)
    hi = min(lo + 1, len(sorted_values) - 1)
    frac = rank - lo
    return sorted_values[lo] * (1.0 - frac) + sorted_values[hi] * frac


def summarize_latencies(latencies_ms: List[float]) -> dict:
    """Return a stats summary dict for a list of latencies in milliseconds."""
    vals = sorted(latencies_ms)
    if not vals:
        return {"count": 0}
    return {
        "count": len(vals),
        "mean_ms": round(statistics.fmean(vals), 3),
        "min_ms": round(vals[0], 3),
        "p50_ms": round(percentile(vals, 50), 3),
        "p90_ms": round(percentile(vals, 90), 3),
        "p99_ms": round(percentile(vals, 99), 3),
        "max_ms": round(vals[-1], 3),
    }


def timed_call(fn, *args, **kwargs):
    """Call fn, return (result, elapsed_ms)."""
    t0 = time.perf_counter_ns()
    result = fn(*args, **kwargs)
    elapsed_ms = (time.perf_counter_ns() - t0) / 1e6
    return result, elapsed_ms


def run_query_workload(
    client: LagoonBenchClient,
    namespace: str,
    queries: Iterable[dict],
    warmup: int = 5,
) -> dict:
    """Closed-loop sequential workload: issue each query, record latency.

    Note on methodology: this is a *closed-loop* measurement (one in-flight
    request).  It measures service latency under no contention and is subject
    to coordinated omission if you reinterpret it as throughput.  We report it
    strictly as per-request latency; see docs/benchmark-guide.md.
    """
    queries = list(queries)
    for q in queries[: min(warmup, len(queries))]:
        client.query(namespace, q)

    client_ms: List[float] = []
    server_ms: List[float] = []
    for q in queries:
        result, elapsed = timed_call(client.query, namespace, q)
        client_ms.append(elapsed)
        took = result.get("took_ms")
        if isinstance(took, (int, float)):
            server_ms.append(float(took))

    out = {"client_latency": summarize_latencies(client_ms)}
    if server_ms:
        out["server_latency"] = summarize_latencies(server_ms)
    return out


# --------------------------------------------------------------------------
# Prometheus text-format parsing (for cache hit rates etc.)
# --------------------------------------------------------------------------


def parse_prometheus_text(text: str) -> Dict[str, float]:
    """Parse Prometheus exposition text into {metric{labels}: value}.

    Keys preserve the label string verbatim, e.g.
    ``lagoon_cache_hits_total{tier="memory"}``.
    Only counter/gauge sample lines are parsed; histograms come through as
    their individual ``_bucket``/``_sum``/``_count`` series.
    """
    out: Dict[str, float] = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        # Format: name{labels} value [timestamp]  OR  name value [timestamp]
        try:
            if "}" in line:
                name_labels, rest = line.split("}", 1)
                key = name_labels + "}"
                value_str = rest.strip().split()[0]
            else:
                parts = line.split()
                key, value_str = parts[0], parts[1]
            out[key] = float(value_str)
        except (ValueError, IndexError):
            continue
    return out


def metric_sum(samples: Dict[str, float], name_prefix: str) -> float:
    """Sum all samples whose key starts with the given metric name."""
    total = 0.0
    for key, value in samples.items():
        bare = key.split("{", 1)[0]
        if bare == name_prefix:
            total += value
    return total


# --------------------------------------------------------------------------
# Reporting
# --------------------------------------------------------------------------


@dataclass
class BenchReport:
    """Collects benchmark results plus environment metadata for honest,
    reproducible reporting."""

    name: str
    params: Dict[str, Any] = field(default_factory=dict)
    results: Dict[str, Any] = field(default_factory=dict)

    def environment(self) -> dict:
        return {
            "timestamp_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "hostname": socket.gethostname(),
            "platform": platform.platform(),
            "python": platform.python_version(),
            "cpu_count": os.cpu_count(),
            "lagoon_url": DEFAULT_BASE_URL,
            "note": (
                "Client-observed latencies include HTTP/JSON overhead. "
                "Record storage backend (filesystem / MinIO / S3), instance "
                "type, and network locality alongside these results."
            ),
        }

    def to_dict(self) -> dict:
        return {
            "benchmark": self.name,
            "params": self.params,
            "environment": self.environment(),
            "results": self.results,
        }

    def save(self, out_dir: str) -> str:
        os.makedirs(out_dir, exist_ok=True)
        path = os.path.join(out_dir, f"{self.name}.json")
        with open(path, "w", encoding="utf-8") as f:
            json.dump(self.to_dict(), f, indent=2)
        return path

    def print_summary(self) -> None:
        print(f"\n=== {self.name} ===")
        print(json.dumps(self.results, indent=2))
