"""Shoal example app: semantic / hybrid search over sample articles.

A deliberately minimal web app built only on the Python standard library
plus the Shoal Python SDK. It seeds a demo namespace on startup, then
serves a single-page search UI backed by a small JSON endpoint that runs
vector, full-text (BM25), or hybrid queries against Shoal.

Run inside the Docker Compose stack, or directly:

    SHOAL_URL=http://localhost:8080 SHOAL_API_KEY=dev-root-key python app.py
"""

from __future__ import annotations

import json
import os
import traceback
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

from shoal import Client

import seed as seeder
from embed import embed

NAMESPACE = seeder.NAMESPACE

CLIENT = Client(
    base_url=os.environ.get("SHOAL_URL", "http://localhost:8080"),
    api_key=os.environ.get("SHOAL_API_KEY", "dev-root-key"),
)

INCLUDE_ATTRIBUTES = ["title", "body", "category", "year"]

INDEX_HTML = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Shoal demo &mdash; article search</title>
<style>
  :root { color-scheme: light dark; }
  body { font-family: system-ui, sans-serif; max-width: 46rem; margin: 2rem auto; padding: 0 1rem; line-height: 1.5; }
  h1 { font-size: 1.4rem; }
  form { display: flex; gap: .5rem; margin-bottom: 1rem; flex-wrap: wrap; }
  input[type=search] { flex: 1 1 16rem; padding: .5rem .7rem; font-size: 1rem; border: 1px solid #8884; border-radius: 6px; }
  select, button { padding: .5rem .7rem; font-size: 1rem; border: 1px solid #8884; border-radius: 6px; background: inherit; }
  button { cursor: pointer; }
  .hit { border: 1px solid #8883; border-radius: 8px; padding: .7rem .9rem; margin: .6rem 0; }
  .hit h3 { margin: 0 0 .25rem; font-size: 1.05rem; }
  .meta { font-size: .8rem; opacity: .7; }
  .err { color: #c33; white-space: pre-wrap; }
</style>
</head>
<body>
<h1>Shoal demo: search 24 sample articles</h1>
<p>Try <em>“durable writes after a crash”</em>, <em>“ocean wildlife”</em>, or <em>“bread fermentation”</em>.
Switch modes to compare vector, BM25 full-text, and hybrid (RRF) ranking.</p>
<form id="f">
  <input type="search" id="q" placeholder="Search articles..." autofocus>
  <select id="mode">
    <option value="hybrid">hybrid (RRF)</option>
    <option value="vector">vector</option>
    <option value="text">full-text (BM25)</option>
  </select>
  <button type="submit">Search</button>
</form>
<div id="out"></div>
<script>
const f = document.getElementById('f');
const out = document.getElementById('out');
f.addEventListener('submit', async (e) => {
  e.preventDefault();
  const q = document.getElementById('q').value.trim();
  const mode = document.getElementById('mode').value;
  if (!q) return;
  out.innerHTML = '<p>Searching…</p>';
  try {
    const res = await fetch(`/api/search?q=${encodeURIComponent(q)}&mode=${mode}`);
    const data = await res.json();
    if (!res.ok) throw new Error(data.error || res.statusText);
    if (!data.results.length) { out.innerHTML = '<p>No results.</p>'; return; }
    out.innerHTML = data.results.map(r => `
      <div class="hit">
        <h3>${escapeHtml(r.title || r.id)}</h3>
        <div>${escapeHtml(r.body || '')}</div>
        <div class="meta">id=${escapeHtml(r.id)} &middot; ${escapeHtml(String(r.category || ''))} &middot; ${escapeHtml(String(r.year || ''))} &middot; score=${Number(r.score).toFixed(4)}</div>
      </div>`).join('');
  } catch (err) {
    out.innerHTML = `<p class="err">${escapeHtml(String(err))}</p>`;
  }
});
function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
}
</script>
</body>
</html>
"""


def run_search(query: str, mode: str, top_k: int = 10) -> list[dict]:
    ns = CLIENT.namespace(NAMESPACE)
    kwargs: dict = {"top_k": top_k, "include_attributes": INCLUDE_ATTRIBUTES}
    if mode in ("vector", "hybrid"):
        kwargs["vector"] = embed(query)
    if mode in ("text", "hybrid"):
        kwargs["text"] = query
    if mode == "hybrid":
        kwargs["fusion"] = "rrf"
    result = ns.query(**kwargs)
    rows = getattr(result, "rows", result)
    out: list[dict] = []
    for row in rows:
        if isinstance(row, dict):
            rid = row.get("id")
            score = row.get("score")
            attrs = row.get("attributes") or {}
        else:
            rid = getattr(row, "id", None)
            score = getattr(row, "score", None)
            attrs = getattr(row, "attributes", None) or {}
        out.append({"id": rid, "score": score, **dict(attrs)})
    return out


class Handler(BaseHTTPRequestHandler):
    server_version = "ShoalDemo/0.1"

    def _send(self, status: int, body: bytes, content_type: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _send_json(self, status: int, payload: dict) -> None:
        self._send(status, json.dumps(payload).encode("utf-8"), "application/json; charset=utf-8")

    def do_GET(self) -> None:  # noqa: N802 - stdlib API
        parsed = urlparse(self.path)
        if parsed.path == "/" or parsed.path == "/index.html":
            self._send(200, INDEX_HTML.encode("utf-8"), "text/html; charset=utf-8")
            return
        if parsed.path == "/healthz":
            self._send_json(200, {"status": "ok"})
            return
        if parsed.path == "/api/search":
            params = parse_qs(parsed.query)
            query = (params.get("q") or [""])[0].strip()
            mode = (params.get("mode") or ["hybrid"])[0]
            if mode not in ("hybrid", "vector", "text"):
                self._send_json(400, {"error": f"unknown mode {mode!r}"})
                return
            if not query:
                self._send_json(400, {"error": "missing query parameter 'q'"})
                return
            try:
                results = run_search(query, mode)
                self._send_json(200, {"mode": mode, "results": results})
            except Exception as exc:  # noqa: BLE001 - report to the UI
                traceback.print_exc()
                self._send_json(502, {"error": f"search failed: {exc}"})
            return
        self._send_json(404, {"error": "not found"})

    def log_message(self, fmt: str, *args: object) -> None:
        print(f"[app] {self.address_string()} {fmt % args}", flush=True)


def main() -> None:
    seeder.seed(CLIENT)
    port = int(os.environ.get("APP_LISTEN_PORT", "8000"))
    server = ThreadingHTTPServer(("0.0.0.0", port), Handler)
    print(f"[app] example app listening on http://0.0.0.0:{port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
