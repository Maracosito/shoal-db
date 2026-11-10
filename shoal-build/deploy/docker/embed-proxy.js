/**
 * Shoal AI Embed Proxy v1.2
 *
 * Transparent proxy that sits in front of Shoal and automatically generates
 * embeddings via Gemini for document upserts and queries.
 *
 * - Intercepts POST /v1/.../documents  → embeds text fields → adds vector → forwards
 * - Intercepts POST /v1/.../query      → embeds text query  → adds vector + fusion → forwards
 * - Automatic model fallback: gemini-embedding-2 → gemini-embedding-001
 * - Graceful BM25 degradation: if embedding fails (offline/quota), forwards text-only
 *   to Shoal so upserts and BM25 searches never break the workflow.
 */

const http = require("http");

const GEMINI_API_KEY = process.env.GEMINI_API_KEY;
const SHOAL_URL = process.env.SHOAL_URL || "http://api:8080";
const PROXY_PORT = parseInt(process.env.PROXY_PORT || "8090", 10);
const EMBED_MODEL = process.env.EMBED_MODEL || "gemini-embedding-2";
const FALLBACK_EMBED_MODELS = [
  EMBED_MODEL,
  "gemini-embedding-2",
  "gemini-embedding-001",
].filter((m, idx, arr) => Boolean(m) && arr.indexOf(m) === idx);
const EMBED_DIMS = parseInt(process.env.EMBED_DIMENSIONS || "768", 10);
const EMBED_TIMEOUT_MS = parseInt(process.env.EMBED_TIMEOUT_MS || "10000", 10);

function embedUrl(model) {
  return `https://generativelanguage.googleapis.com/v1beta/models/${model}:embedContent?key=${GEMINI_API_KEY}`;
}

function batchEmbedUrl(model) {
  return `https://generativelanguage.googleapis.com/v1beta/models/${model}:batchEmbedContents?key=${GEMINI_API_KEY}`;
}

// ── Embedding ──────────────────────────────────────────────────────────

/** Concatenate all text fields of a document into a single string. */
function docToText(doc) {
  if (!doc.text || typeof doc.text !== "object") return null;
  const parts = [];
  for (const [field, value] of Object.entries(doc.text)) {
    if (typeof value === "string" && value.length > 0) {
      parts.push(`${field}: ${value}`);
    }
  }
  return parts.length > 0 ? parts.join("\n").slice(0, 8000) : null;
}

/** Call Gemini to embed a single text with automatic model fallback. */
async function embedOne(text) {
  let lastErr = null;
  for (const model of FALLBACK_EMBED_MODELS) {
    try {
      const res = await fetch(embedUrl(model), {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          content: { parts: [{ text }] },
          outputDimensionality: EMBED_DIMS,
        }),
        signal: AbortSignal.timeout(EMBED_TIMEOUT_MS),
      });
      if (!res.ok) {
        const errText = await res.text();
        lastErr = new Error(`Gemini embed (${model}) ${res.status}: ${errText}`);
        continue;
      }
      const data = await res.json();
      if (data?.embedding?.values) {
        return data.embedding.values;
      }
      lastErr = new Error(`Gemini embed (${model}) returned empty vector`);
    } catch (err) {
      lastErr = err;
    }
  }
  throw lastErr || new Error("All Gemini embedding models failed");
}

/** Batch embed multiple texts (up to 100 per chunk) with fallback. */
async function embedBatch(texts) {
  if (texts.length === 0) return [];
  if (texts.length === 1) return [await embedOne(texts[0])];

  let lastErr = null;
  for (const model of FALLBACK_EMBED_MODELS) {
    try {
      const requests = texts.map((text) => ({
        model: `models/${model}`,
        content: { parts: [{ text }] },
        outputDimensionality: EMBED_DIMS,
      }));

      const res = await fetch(batchEmbedUrl(model), {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ requests }),
        signal: AbortSignal.timeout(EMBED_TIMEOUT_MS * 2),
      });
      if (!res.ok) {
        const errText = await res.text();
        lastErr = new Error(`Gemini batch embed (${model}) ${res.status}: ${errText}`);
        continue;
      }
      const data = await res.json();
      if (Array.isArray(data?.embeddings)) {
        return data.embeddings.map((e) => e.values);
      }
      lastErr = new Error(`Gemini batch embed (${model}) returned invalid payload`);
    } catch (err) {
      lastErr = err;
    }
  }
  throw lastErr || new Error("All Gemini batch embedding models failed");
}

// ── Request Interceptors ───────────────────────────────────────────────

/** Intercept document upserts: auto-embed text fields into vectors. */
async function interceptDocuments(body) {
  if (!body.documents || !Array.isArray(body.documents)) {
    return { body, mode: "passthrough" };
  }

  const toEmbed = [];
  const embedIndices = [];

  for (let i = 0; i < body.documents.length; i++) {
    const doc = body.documents[i];
    if (!doc.vector) {
      const text = docToText(doc);
      if (text) {
        toEmbed.push(text);
        embedIndices.push(i);
      }
    }
  }

  if (toEmbed.length > 0) {
    try {
      console.log(`[embed] Generating ${toEmbed.length} embeddings for documents...`);
      const vectors = await embedBatch(toEmbed);
      for (let j = 0; j < embedIndices.length; j++) {
        if (Array.isArray(vectors[j]) && vectors[j].length === EMBED_DIMS) {
          body.documents[embedIndices[j]].vector = vectors[j];
        }
      }
      console.log(`[embed] ✓ ${vectors.length} vectors added (${EMBED_DIMS}d)`);
      return { body, mode: "hybrid-vector" };
    } catch (err) {
      console.warn(`[embed] ⚠ Embedding failed (${err.message}) — degrading gracefully to BM25-only upsert`);
      return { body, mode: "bm25-fallback" };
    }
  }

  return { body, mode: "passthrough" };
}

/** Intercept queries: auto-embed text query, enable hybrid fusion. */
async function interceptQuery(body) {
  const queryText = body.text?.query || (typeof body.text === "string" ? body.text : null);

  // Always normalize string text to TextQuery struct required by Rust server
  if (typeof body.text === "string") {
    body.text = { query: body.text };
  }

  if (queryText && !body.vector) {
    try {
      console.log(`[embed] Embedding query: "${queryText.slice(0, 80)}..."`);
      const vector = await embedOne(queryText);
      body.vector = vector;
      if (!body.fusion) {
        body.fusion = { method: "rrf", rrf_k: 60.0 };
      }
      console.log(`[embed] ✓ Hybrid query: BM25 + vector (${EMBED_DIMS}d) + RRF fusion`);
      return { body, mode: "hybrid-rrf" };
    } catch (err) {
      console.warn(`[embed] ⚠ Query embedding failed (${err.message}) — executing BM25-only search`);
      return { body, mode: "bm25-fallback" };
    }
  }

  return { body, mode: "passthrough" };
}

// ── Proxy Core ─────────────────────────────────────────────────────────

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

async function forward(req, bodyBuffer) {
  const url = `${SHOAL_URL}${req.url}`;
  const headers = {
    "content-type": req.headers["content-type"] || "application/json",
  };
  if (req.headers["authorization"]) {
    headers["authorization"] = req.headers["authorization"];
  }

  const opts = {
    method: req.method,
    headers,
  };
  if (!["GET", "HEAD"].includes(req.method) && bodyBuffer.length > 0) {
    opts.body = bodyBuffer;
  }

  const res = await fetch(url, opts);
  const respBody = Buffer.from(await res.arrayBuffer());
  return { status: res.status, headers: Object.fromEntries(res.headers), body: respBody };
}

async function handleRequest(req, res) {
  const start = Date.now();
  let embedMode = "passthrough";

  try {
    let bodyBuffer = await readBody(req);

    const isDocuments = req.method === "POST" && req.url.includes("/documents");
    const isQuery = req.method === "POST" && req.url.includes("/query");

    if ((isDocuments || isQuery) && bodyBuffer.length > 0) {
      try {
        let body = JSON.parse(bodyBuffer.toString());
        if (GEMINI_API_KEY) {
          if (isDocuments) {
            const out = await interceptDocuments(body);
            body = out.body;
            embedMode = out.mode;
          } else if (isQuery) {
            const out = await interceptQuery(body);
            body = out.body;
            embedMode = out.mode;
          }
        } else if (isQuery && typeof body.text === "string") {
          body.text = { query: body.text };
          embedMode = "bm25-no-key";
        }
        bodyBuffer = Buffer.from(JSON.stringify(body));
      } catch (parseErr) {
        console.warn(`[embed] Non-JSON body or parse warning: ${parseErr.message}`);
      }
    }

    let upstream = await forward(req, bodyBuffer);

    // If a namespace has different vector dimensions (e.g. legacy 64d demo namespace),
    // automatically retry the query/upsert in BM25-only mode instead of failing with 400!
    if (upstream.status === 400 && (isQuery || isDocuments)) {
      const errText = upstream.body.toString();
      if (errText.includes("dimension") && bodyBuffer.length > 0) {
        console.warn(`[embed] ⚠ Dimension mismatch detected — retrying in BM25-only mode`);
        const fallbackBody = JSON.parse(bodyBuffer.toString());
        if (isQuery) {
          delete fallbackBody.vector;
          delete fallbackBody.fusion;
        } else if (isDocuments && Array.isArray(fallbackBody.documents)) {
          for (const d of fallbackBody.documents) delete d.vector;
        }
        embedMode = "bm25-dim-fallback";
        upstream = await forward(req, Buffer.from(JSON.stringify(fallbackBody)));
      }
    }

    res.writeHead(upstream.status, {
      "content-type": upstream.headers["content-type"] || "application/json",
      "x-embed-proxy": "shoal-ai-v1.2",
      "x-embed-mode": embedMode,
      "x-embed-ms": `${Date.now() - start}`,
    });
    res.end(upstream.body);
  } catch (err) {
    console.error(`[error] ${req.method} ${req.url}: ${err.message}`);
    res.writeHead(502, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: { code: "proxy_error", message: err.message } }));
  }
}

// ── Start ──────────────────────────────────────────────────────────────

const server = http.createServer(handleRequest);
server.listen(PROXY_PORT, "0.0.0.0", () => {
  console.log(`
╔══════════════════════════════════════════════════╗
║          Shoal AI Embed Proxy v1.2               ║
║──────────────────────────────────────────────────║
║  Proxy:     http://0.0.0.0:${PROXY_PORT}                  ║
║  Upstream:  ${SHOAL_URL.padEnd(36)}║
║  Models:    ${FALLBACK_EMBED_MODELS.join(" -> ").slice(0, 36).padEnd(36)}║
║  Dims:      ${String(EMBED_DIMS).padEnd(36)}║
║  AI Key:    ${(GEMINI_API_KEY ? "✓ configured" : "✗ MISSING").padEnd(36)}║
╚══════════════════════════════════════════════════╝
`);
});
