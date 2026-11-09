# Code Search Demo — Branching Workspaces

This demo indexes a small bundled Python project (`sample-repo/`, a fictional
tide-gauge data library called **tidegauge**) for hybrid code search, then
uses **copy-on-write namespace branching** to model git-style branches:

```
code-main ──branch──▶ code-feature ──branch──▶ code-experiment
```

Branching is instant regardless of namespace size because the branch only
*references* the source's immutable segments in object storage; new writes
land in branch-private WAL/segments. The demo proves four properties:

1. **Branch sees source data** immediately after branching.
2. **Writes to the branch don't leak into the source** (the feature branch
   adds a `forecast.py` module; main never sees it).
3. **Writes to the source don't leak into the branch** (main gains release
   notes after the branch point; the branch never sees them).
4. **Deleting a branch never deletes shared data** still referenced by the
   source or sibling branches (refcounted segments).

## Run it

With the dev stack up:

```bash
pip install -r demos/requirements.txt

# Index sample-repo into namespace `code-main`
python demos/code-search/ingest.py

# Run the full branching walkthrough (branches, isolation checks, deletion)
python demos/code-search/branch_demo.py

# Search any namespace ad hoc
python demos/code-search/search.py "parse a csv of water level readings"
python demos/code-search/search.py "predict future tide levels" --namespace code-feature
python demos/code-search/search.py "moving average" --kind function --lang python
```

`branch_demo.py` exits non-zero if any isolation property fails, so it
doubles as a smoke test against a running server.

## How indexing works

`ingest.py` walks a directory, splits Python files into chunks at top-level
`def`/`class` boundaries (plus a module-header chunk), treats Markdown files
as whole-file chunks, embeds each chunk with the pluggable provider
(`hash` by default — offline, lexical), and upserts documents with stable IDs
of the form `path::symbol`, so re-running ingest is idempotent. Each chunk
carries filterable attributes: `path`, `symbol`, `kind`
(`module|function|class|doc`), `lang`, and `start_line`.
