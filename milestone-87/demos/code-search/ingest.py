#!/usr/bin/env python3
"""Index a source directory into a Lagoon namespace for code search.

Splits Python files into top-level def/class chunks (plus a module-header
chunk) and Markdown files into whole-file chunks. Document IDs are
`relative/path::symbol`, so re-ingesting is idempotent (upserts overwrite).

Usage:
    python demos/code-search/ingest.py                          # sample-repo -> code-main
    python demos/code-search/ingest.py --root path/to/src --namespace my-code
"""
from __future__ import annotations

import argparse
import pathlib
import re
import sys
from typing import List

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from common.embeddings import get_provider
from common.lagoon_client import LagoonClient

SAMPLE_REPO = pathlib.Path(__file__).resolve().parent / "sample-repo"
TOP_LEVEL_RE = re.compile(r"^(def|class)\s+([A-Za-z_][A-Za-z0-9_]*)")


def chunk_python(text: str) -> List[dict]:
    """Split a Python source file into chunks at top-level def/class lines."""
    lines = text.splitlines()
    boundaries = []
    for i, line in enumerate(lines):
        match = TOP_LEVEL_RE.match(line)
        if match:
            boundaries.append((i, match.group(1), match.group(2)))

    chunks: List[dict] = []
    if not boundaries:
        body = text.strip()
        if body:
            chunks.append({"symbol": "(module)", "kind": "module", "start_line": 1, "code": body})
        return chunks

    head = "\n".join(lines[: boundaries[0][0]]).strip()
    if head:
        chunks.append({"symbol": "(module)", "kind": "module", "start_line": 1, "code": head})
    for j, (i, keyword, name) in enumerate(boundaries):
        end = boundaries[j + 1][0] if j + 1 < len(boundaries) else len(lines)
        chunks.append(
            {
                "symbol": name,
                "kind": "function" if keyword == "def" else "class",
                "start_line": i + 1,
                "code": "\n".join(lines[i:end]).rstrip(),
            }
        )
    return chunks


def collect_documents(root: pathlib.Path) -> List[dict]:
    docs: List[dict] = []
    for path in sorted(root.rglob("*")):
        if not path.is_file() or path.suffix not in {".py", ".md"}:
            continue
        rel = path.relative_to(root).as_posix()
        text = path.read_text(encoding="utf-8")
        if path.suffix == ".py":
            for chunk in chunk_python(text):
                docs.append(
                    {
                        "id": f"{rel}::{chunk['symbol']}",
                        "path": rel,
                        "symbol": chunk["symbol"],
                        "kind": chunk["kind"],
                        "lang": "python",
                        "start_line": chunk["start_line"],
                        "code": chunk["code"],
                    }
                )
        else:
            docs.append(
                {
                    "id": f"{rel}::(doc)",
                    "path": rel,
                    "symbol": "(doc)",
                    "kind": "doc",
                    "lang": "markdown",
                    "start_line": 1,
                    "code": text.strip(),
                }
            )
    return docs


def ingest_directory(
    client: LagoonClient,
    namespace: str,
    root: pathlib.Path,
    provider,
    *,
    reset: bool = False,
    create: bool = True,
) -> int:
    """Index `root` into `namespace`. Returns the number of chunks upserted.

    With reset=False and create=False this only upserts (used to overlay
    changes onto an existing branch namespace).
    """
    docs = collect_documents(root)
    if not docs:
        return 0
    vectors = provider.embed([f"{d['path']} {d['symbol']}\n{d['code']}" for d in docs])
    for doc, vector in zip(docs, vectors):
        doc["vector"] = vector
    if reset:
        client.reset_namespace(
            namespace, dims=provider.dims, metric="cosine",
            text_fields=["code", "symbol", "path"],
        )
    elif create and not client.namespace_exists(namespace):
        client.create_namespace(
            namespace, dims=provider.dims, metric="cosine",
            text_fields=["code", "symbol", "path"],
        )
    return client.upsert(namespace, docs)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--root", default=str(SAMPLE_REPO))
    ap.add_argument("--namespace", default="code-main")
    ap.add_argument("--provider", default=None)
    args = ap.parse_args()

    client = LagoonClient()
    provider = get_provider(args.provider)
    root = pathlib.Path(args.root)
    if not root.is_dir():
        print(f"error: {root} is not a directory", file=sys.stderr)
        return 1
    count = ingest_directory(client, args.namespace, root, provider, reset=True)
    print(f"indexed {count} chunks from {root} into namespace {args.namespace!r}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
