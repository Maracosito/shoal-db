#!/usr/bin/env python3
"""End-to-end walkthrough of copy-on-write namespace branching.

Steps:
  1. (Re)index sample-repo into `code-main`.
  2. Branch code-main -> code-feature; verify the branch sees source data.
  3. Apply the feature overlay (new forecast.py, updated stats.py) to the
     branch only; verify main is unaffected.
  4. Write release notes to main; verify the branch is unaffected.
  5. Branch code-feature -> code-experiment (multi-level); add a doc there;
     verify neither parent sees it.
  6. Delete code-experiment; verify code-feature and code-main still work.

Exits non-zero if any isolation property fails.
"""
from __future__ import annotations

import pathlib
import sys
from typing import List

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from common.embeddings import get_provider
from common.lagoon_client import LagoonClient
from ingest import SAMPLE_REPO, ingest_directory  # noqa: E402  (same directory)

FEATURE_OVERLAY = pathlib.Path(__file__).resolve().parent / "feature-overlay"

MAIN = "code-main"
FEATURE = "code-feature"
EXPERIMENT = "code-experiment"

_failures: List[str] = []


def check(condition: bool, label: str) -> None:
    status = "PASS" if condition else "FAIL"
    print(f"  [{status}] {label}")
    if not condition:
        _failures.append(label)


def hit_paths(hits: List[dict]) -> set:
    return {h.get("attributes", {}).get("path") for h in hits}


def hit_ids(hits: List[dict]) -> set:
    return {h["id"] for h in hits}


def hybrid(client: LagoonClient, provider, namespace: str, query: str, top_k: int = 8) -> List[dict]:
    vector = provider.embed([query])[0]
    return client.query(
        namespace, text=query, vector=vector, mode="hybrid",
        fusion={"method": "rrf", "k": 60},
        fields={"symbol": 2.0, "code": 1.0, "path": 1.5},
        top_k=top_k, include_attributes=["path", "symbol", "kind"],
    )


def main() -> int:
    client = LagoonClient()
    provider = get_provider()

    # -- 1. baseline index ---------------------------------------------------
    print(f"\n== 1. indexing sample-repo into {MAIN!r}")
    count = ingest_directory(client, MAIN, SAMPLE_REPO, provider, reset=True)
    print(f"  indexed {count} chunks")
    for ns in (FEATURE, EXPERIMENT):
        client.delete_namespace(ns, missing_ok=True)

    # -- 2. branch and verify shared visibility -------------------------------
    print(f"\n== 2. branching {MAIN!r} -> {FEATURE!r}")
    client.branch_namespace(MAIN, FEATURE)
    base_query = "parse a csv file of water level readings"
    main_hits = hybrid(client, provider, MAIN, base_query)
    branch_hits = hybrid(client, provider, FEATURE, base_query)
    check(len(branch_hits) > 0, "branch returns results immediately after branching")
    check(
        "tidegauge/parser.py" in hit_paths(branch_hits),
        "branch sees source data (parser.py findable via branch)",
    )
    check(hit_ids(main_hits) == hit_ids(branch_hits), "branch initially matches source results")

    # -- 3. write to branch; source must be unaffected -------------------------
    print(f"\n== 3. applying feature overlay to {FEATURE!r} only")
    # stats.py changed on the feature branch: drop the old chunks for that
    # path in the branch before overlaying, so removed symbols don't linger.
    client.delete_documents(FEATURE, filter=["path", "Eq", "tidegauge/stats.py"])
    overlay_count = ingest_directory(
        client, FEATURE, FEATURE_OVERLAY, provider, reset=False, create=False
    )
    print(f"  overlaid {overlay_count} chunks")
    forecast_query = "predict future tide levels with harmonic analysis"
    feature_hits = hybrid(client, provider, FEATURE, forecast_query)
    main_hits = hybrid(client, provider, MAIN, forecast_query)
    check(
        "tidegauge/forecast.py" in hit_paths(feature_hits),
        "feature branch finds the new forecast.py module",
    )
    check(
        "tidegauge/forecast.py" not in hit_paths(main_hits),
        "source namespace does NOT see branch-only forecast.py",
    )
    check(
        any(h.get("attributes", {}).get("symbol") == "harmonic_fit" for h in feature_hits),
        "feature branch finds the updated harmonic_fit in stats.py",
    )

    # -- 4. write to source; branch must be unaffected --------------------------
    print(f"\n== 4. writing release notes to {MAIN!r} only")
    notes = {
        "id": "CHANGELOG.md::(doc)",
        "path": "CHANGELOG.md",
        "symbol": "(doc)",
        "kind": "doc",
        "lang": "markdown",
        "start_line": 1,
        "code": "# Release notes\n\n## 0.1.1\n- Fixed timezone handling in the CSV parser.",
        "vector": provider.embed(["release notes changelog 0.1.1 timezone fix csv parser"])[0],
    }
    client.upsert(MAIN, [notes])
    notes_query = "release notes changelog"
    check(
        "CHANGELOG.md::(doc)" in hit_ids(hybrid(client, provider, MAIN, notes_query)),
        "source sees its own post-branch write (read-your-writes)",
    )
    check(
        "CHANGELOG.md::(doc)" not in hit_ids(hybrid(client, provider, FEATURE, notes_query)),
        "branch does NOT see writes made to source after branching",
    )

    # -- 5. multi-level branch ----------------------------------------------------
    print(f"\n== 5. branching {FEATURE!r} -> {EXPERIMENT!r} (multi-level)")
    client.branch_namespace(FEATURE, EXPERIMENT)
    ml_doc = {
        "id": "tidegauge/ml.py::train_model",
        "path": "tidegauge/ml.py",
        "symbol": "train_model",
        "kind": "function",
        "lang": "python",
        "start_line": 1,
        "code": "def train_model(readings):\n    \"\"\"Fit a gradient boosting model to residual tide levels.\"\"\"\n    ...",
        "vector": provider.embed(["train gradient boosting model residual tide levels"])[0],
    }
    client.upsert(EXPERIMENT, [ml_doc])
    ml_query = "gradient boosting machine learning model"
    check(
        "tidegauge/ml.py::train_model" in hit_ids(hybrid(client, provider, EXPERIMENT, ml_query)),
        "second-level branch sees its own write",
    )
    check(
        "tidegauge/ml.py::train_model" not in hit_ids(hybrid(client, provider, FEATURE, ml_query)),
        "first-level branch does NOT see grandchild's write",
    )
    check(
        "tidegauge/forecast.py" in hit_paths(hybrid(client, provider, EXPERIMENT, forecast_query)),
        "second-level branch inherits first-level branch's data",
    )

    # -- 6. branch deletion safety ---------------------------------------------------
    print(f"\n== 6. deleting {EXPERIMENT!r}; shared data must survive")
    client.delete_namespace(EXPERIMENT)
    check(not client.namespace_exists(EXPERIMENT), "deleted branch is gone")
    check(
        "tidegauge/forecast.py" in hit_paths(hybrid(client, provider, FEATURE, forecast_query)),
        "sibling/parent branch still queryable after branch deletion",
    )
    check(
        "tidegauge/parser.py" in hit_paths(hybrid(client, provider, MAIN, base_query)),
        "source namespace still queryable after branch deletion",
    )

    # -- summary -----------------------------------------------------------------------
    print()
    if _failures:
        print(f"RESULT: {len(_failures)} check(s) FAILED:")
        for f in _failures:
            print(f"  - {f}")
        return 1
    print("RESULT: all branching isolation checks passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
