"""Seed the demo namespace with sample articles.

Idempotent: safe to run repeatedly. Upserts overwrite documents with the
same IDs, so re-seeding converges to the same state.
"""

from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path

from shoal import Shoal

from embed import DIM, embed

NAMESPACE = "demo-articles"
DATA_PATH = Path(__file__).parent / "data" / "articles.jsonl"


def _ns_name(ns: object) -> str:
    """Tolerate both model objects and plain dicts from list_namespaces()."""
    if isinstance(ns, dict):
        return str(ns.get("name", ""))
    return str(getattr(ns, "name", ns))


def wait_for_api(client: Shoal, timeout: float = 120.0) -> None:
    deadline = time.monotonic() + timeout
    last_err: Exception | None = None
    while time.monotonic() < deadline:
        try:
            client.health()
            return
        except Exception as exc:  # noqa: BLE001 - retry until healthy
            last_err = exc
            time.sleep(2.0)
    raise RuntimeError(f"Shoal API did not become healthy in {timeout}s: {last_err}")


def load_documents() -> list[dict]:
    docs: list[dict] = []
    for line in DATA_PATH.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        rec = json.loads(line)
        docs.append(
            {
                "id": rec["id"],
                "vector": embed(f"{rec['title']} {rec['body']}"),
                "text": {
                    "title": rec["title"],
                    "body": rec["body"],
                },
                "attributes": {
                    "title": rec["title"],
                    "body": rec["body"],
                    "category": rec["category"],
                    "year": rec["year"],
                },
            }
        )
    return docs


def seed(client: Shoal) -> int:
    wait_for_api(client)
    existing = {_ns_name(ns) for ns in client.list_namespaces()}
    if NAMESPACE not in existing:
        client.create_namespace(NAMESPACE, dimensions=DIM, distance_metric="cosine")
        print(f"created namespace {NAMESPACE!r}", flush=True)
    docs = load_documents()
    ns = client.namespace(NAMESPACE)
    ns.upsert(documents=docs)
    print(f"seeded {len(docs)} documents into {NAMESPACE!r}", flush=True)
    return len(docs)


def main() -> int:
    client = Shoal(
        base_url=os.environ.get("SHOAL_URL", "http://localhost:8080"),
        api_key=os.environ.get("SHOAL_API_KEY", "dev-root-key"),
    )
    seed(client)
    return 0


if __name__ == "__main__":
    sys.exit(main())
