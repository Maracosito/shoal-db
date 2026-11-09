/**
 * Shoal AI Embed Proxy
 * 
 * Transparent proxy that sits in front of Shoal and automatically generates
 * embeddings via Gemini for document upserts and queries.
 * 
 * - Intercepts POST /v1/.../documents  → embeds text fields → adds vector → forwards
 * - Intercepts POST /v1/.../query      → embeds text query  → adds vector + fusion → forwards
 * - Passes through everything else unchanged
 * 
 * Environment:
 *   GEMINI_API_KEY    - Gemini API key for text-embedding-004
 *   SHOAL_URL         - Upstream Shoal API (default: http://api:8080)
 *   PROXY_PORT        - Listen port (default: 8090)
 *   EMBED_MODEL       - Gemini model (default: text-embedding-004)
 *   EMBED_DIMENSIONS  - Vector dimensions (default: 768)
 */

const http = require("http");

const GEMINI_API_KEY = process.env.GEMINI_API_KEY;
const SHOAL_URL = process.env.SHOAL_URL || "http://api:8080";
const PROXY_PORT = parseInt(process.env.PROXY_PORT || "8090", 10);
const EMBED_MODEL = process.env.EMBED_MODEL || "text-embedding-004";
const EMBED_DIMS = parseInt(process.env.EMBED_DIMENSIONS || "768", 10);

const GEMINI_EMBED_URL = `https://generativelanguage.googleapis.com/v1beta/models/${EMBED_MODEL}:embedContent?key=${GEMINI_API_KEY}`;
const GEMINI_BATCH_URL = `https://generativelanguage.googleapis.com/v1beta/models/${EMBED_MODEL}:batchEmbedContents?key=${GEMINI_API_KEY}`;

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

/** Call Gemini to embed a single text. */
async function embedOne(text) {
  const res = await fetch(GEMINI_EMBED_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      content: { parts: [{ text }] },
      outputDimensionality: EMBED_DIMS,
    }),
  });
  if (!res.ok) {
    const err = await res.text();
    throw new Error(`Gemini embed error ${res.status}: ${err}`);
  }
  const data = await res.json();
  return data.embedding.values;
}

/** Batch embed multiple texts (up to 100). */
async function embedBatch(texts) {
  if (texts.length === 0) return [];
  if (texts.length === 1) return [await embedOne(texts[0])];

  const requests = texts.map((text) => ({
    model: `models/${EMBED_MODEL}`,
    content: { parts: [{ text }] },
    outputDimensionality: EMBED_DIMS,
  }));

  const res = await fetch(GEMINI_BATCH_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ requests }),
  });
  if (!res.ok) {
    const err = await res.text();
    throw new Error(`Gemini batch embed error ${res.status}: ${err}`);
  }
  const data = await res.json();
  return data.embeddings.map((e) => e.values);
}

// ── Request Interceptors ───────────────────────────────────────────────

/** Intercept document upserts: auto-embed text fields into vectors. */
async function interceptDocuments(body) {
  if (!body.documents || !Array.isArray(body.documents)) return body;

  // Collect texts that need embedding (docs with text but no vector)
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
    console.log(`[embed] Generating ${toEmbed.length} embeddings for documents...`);
    const vectors = await embedBatch(toEmbed);
    for (let j = 0; j < embedIndices.length; j++) {
      body.documents[embedIndices[j]].vector = vectors[j];
    }
    console.log(`[embed] ✓ ${vectors.length} vectors added (${EMBED_DIMS}d)`);
  }

  return body;
}

/** Intercept queries: auto-embed text query, enable hybrid fusion. */
async function interceptQuery(body) {
  // If there's a text query but no vector, embed the query text
  const queryText = body.text?.query || (typeof body.text === "string" ? body.text : null);

  if (queryText && !body.vector) {
    console.log(`[embed] Embedding query: "${queryText.slice(0, 80)}..."`);
    const vector = await embedOne(queryText);
    body.vector = vector;

    // Normalize text field to TextQuery format if it was a string
    if (typeof body.text === "string") {
      body.text = { query: body.text };
    }

    // Enable hybrid fusion if both text and vector are now present
    if (!body.fusion) {
      body.fusion = { method: "rrf", rrf_k: 60.0 };
    }
    console.log(`[embed] ✓ Hybrid query: BM25 + vector (${EMBED_DIMS}d) + RRF fusion`);
  }

  return body;
}

// ── Proxy Core ─────────────────────────────────────────────────────────

/** Read the full request body. */
function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

/** Forward a request to Shoal. */
async function forward(req, bodyBuffer) {
  const url = `${SHOAL_URL}${req.url}`;
  const headers = {
    "content-type": req.headers["content-type"] || "application/json",
  };
  // Forward auth header if present
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

/** Main request handler. */
async function handleRequest(req, res) {
  const start = Date.now();

  try {
    let bodyBuffer = await readBody(req);

    // Intercept document upserts and queries
    const isDocuments = req.method === "POST" && req.url.includes("/documents");
    const isQuery = req.method === "POST" && req.url.includes("/query");

    if (GEMINI_API_KEY && (isDocuments || isQuery) && bodyBuffer.length > 0) {
      let body = JSON.parse(bodyBuffer.toString());

      if (isDocuments) {
        body = await interceptDocuments(body);
      } else if (isQuery) {
        body = await interceptQuery(body);
      }

      bodyBuffer = Buffer.from(JSON.stringify(body));
    }

    // Forward to Shoal
    const upstream = await forward(req, bodyBuffer);

    // Send response
    res.writeHead(upstream.status, {
      "content-type": upstream.headers["content-type"] || "application/json",
      "x-embed-proxy": "shoal-ai",
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
║          Shoal AI Embed Proxy v1.0               ║
║──────────────────────────────────────────────────║
║  Proxy:     http://0.0.0.0:${PROXY_PORT}                  ║
║  Upstream:  ${SHOAL_URL.padEnd(36)}║
║  Model:     ${EMBED_MODEL.padEnd(36)}║
║  Dims:      ${String(EMBED_DIMS).padEnd(36)}║
║  AI Key:    ${GEMINI_API_KEY ? "✓ configured" : "✗ MISSING".padEnd(36)}║
╚══════════════════════════════════════════════════╝
`);
});
