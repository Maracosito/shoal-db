import { describe, expect, it } from "vitest";
import { ShoalClient } from "../src/client.js";
import { eq } from "../src/filters.js";
import { instantSleep, mockFetch, type CannedResponse } from "./helpers.js";

function makeClient(responses: CannedResponse[]) {
  const mock = mockFetch(responses);
  const client = new ShoalClient({
    baseUrl: "http://test.local",
    apiKey: "sk-test",
    fetch: mock.fetch,
    sleep: instantSleep,
  });
  return { client, mock };
}

describe("ShoalClient", () => {
  it("creates namespaces with PUT and config payload", async () => {
    const { client, mock } = makeClient([
      { status: 200, body: { name: "articles" } },
    ]);
    const info = await client.createNamespace("articles", {
      vector_dims: 4,
      metric: "cosine",
    });
    expect(info.name).toBe("articles");
    const req = mock.requests[0]!;
    expect(req.method).toBe("PUT");
    expect(req.url).toBe("http://test.local/v1/namespaces/articles");
    expect(req.body).toEqual({ config: { vector_dims: 4, metric: "cosine" } });
  });

  it("URL-encodes namespace names", async () => {
    const { client, mock } = makeClient([{ status: 200, body: { name: "a/b" } }]);
    await client.namespace("a/b").info();
    expect(mock.requests[0]!.url).toBe("http://test.local/v1/namespaces/a%2Fb");
  });

  it("follows pagination when listing namespaces", async () => {
    const { client, mock } = makeClient([
      {
        status: 200,
        body: { namespaces: [{ name: "a" }], next_cursor: "c1" },
      },
      {
        status: 200,
        body: { namespaces: [{ name: "b" }], next_cursor: null },
      },
    ]);
    const all = await client.listNamespaces();
    expect(all.map((n) => n.name)).toEqual(["a", "b"]);
    const url2 = new URL(mock.requests[1]!.url);
    expect(url2.searchParams.get("cursor")).toBe("c1");
  });

  it("upserts documents", async () => {
    const { client, mock } = makeClient([{ status: 200, body: { upserted: 2 } }]);
    const res = await client.namespace("ns").upsert([
      { id: "1", vector: [0.1, 0.2], attributes: { lang: "en" } },
      { id: "2", text: { body: "hello" } },
    ]);
    expect(res.upserted).toBe(2);
    const req = mock.requests[0]!;
    expect(req.method).toBe("POST");
    expect(req.url).toBe("http://test.local/v1/namespaces/ns/documents");
    expect((req.body as { documents: unknown[] }).documents.length).toBe(2);
  });

  it("upserts columns", async () => {
    const { client, mock } = makeClient([{ status: 200, body: { upserted: 2 } }]);
    await client.namespace("ns").upsertColumns({
      ids: ["1", "2"],
      vectors: [
        [0.1, 0.2],
        [0.3, 0.4],
      ],
      attributes: { lang: ["en", "de"] },
    });
    const body = mock.requests[0]!.body as { columns: { ids: string[] } };
    expect(body.columns.ids).toEqual(["1", "2"]);
  });

  it("upsertBatched chunks, sets idempotency keys, and aggregates counts", async () => {
    const { client, mock } = makeClient([{ status: 200, body: { upserted: 2 } }]);
    const documents = Array.from({ length: 6 }, (_, i) => ({ id: `d${i}` }));
    const progress: number[] = [];
    const res = await client.namespace("ns").upsertBatched(documents, {
      batchSize: 2,
      concurrency: 1,
      idempotencyPrefix: "load-42",
      onProgress: (p) => progress.push(p.documentsDone),
    });
    expect(res.upserted).toBe(6);
    expect(mock.requests.length).toBe(3);
    expect(mock.requests[0]!.headers["idempotency-key"]).toBe("load-42:0");
    expect(mock.requests[2]!.headers["idempotency-key"]).toBe("load-42:2");
    expect(progress).toEqual([2, 4, 6]);
  });

  it("deletes by ids and by filter", async () => {
    const { client, mock } = makeClient([
      { status: 200, body: { deleted: 2 } },
      { status: 200, body: { deleted: 5 } },
    ]);
    const ns = client.namespace("ns");
    await ns.deleteByIds(["1", "2"]);
    await ns.deleteByFilter(eq("lang", "de"));
    expect(mock.requests[0]!.body).toEqual({ ids: ["1", "2"] });
    expect(mock.requests[1]!.body).toEqual({
      filter: { op: "eq", field: "lang", value: "de" },
    });
    expect(mock.requests[0]!.url).toBe(
      "http://test.local/v1/namespaces/ns/documents/delete",
    );
  });

  it("runs hybrid queries", async () => {
    const { client, mock } = makeClient([
      {
        status: 200,
        body: {
          results: [{ id: "1", score: 0.9, attributes: { lang: "en" } }],
          plan: "hybrid_rrf",
        },
      },
    ]);
    const res = await client.namespace("ns").query({
      vector: [0.1, 0.2],
      text: "hello world",
      top_k: 5,
      fusion: { method: "rrf", rrf_k: 60 },
      filter: eq("lang", "en"),
    });
    expect(res.results[0]!.id).toBe("1");
    expect(res.plan).toBe("hybrid_rrf");
    const body = mock.requests[0]!.body as Record<string, unknown>;
    expect(body["top_k"]).toBe(5);
    expect(body["fusion"]).toEqual({ method: "rrf", rrf_k: 60 });
  });

  it("runs multi-queries", async () => {
    const { client, mock } = makeClient([
      { status: 200, body: { responses: [{ results: [] }, { results: [] }] } },
    ]);
    const res = await client
      .namespace("ns")
      .multiQuery([{ text: "a" }, { text: "b" }]);
    expect(res.responses.length).toBe(2);
    const body = mock.requests[0]!.body as { queries: unknown[] };
    expect(body.queries.length).toBe(2);
  });

  it("iterates export pages with cursors", async () => {
    const { client, mock } = makeClient([
      {
        status: 200,
        body: { documents: [{ id: "1" }, { id: "2" }], next_cursor: "c1" },
      },
      { status: 200, body: { documents: [{ id: "3" }], next_cursor: null } },
    ]);
    const ids: string[] = [];
    for await (const doc of client.namespace("ns").exportDocuments({ pageSize: 2 })) {
      ids.push(doc.id);
    }
    expect(ids).toEqual(["1", "2", "3"]);
    const url1 = new URL(mock.requests[0]!.url);
    expect(url1.searchParams.get("limit")).toBe("2");
    const url2 = new URL(mock.requests[1]!.url);
    expect(url2.searchParams.get("cursor")).toBe("c1");
  });

  it("copies, branches, warms, and pins namespaces", async () => {
    const { client, mock } = makeClient([
      { status: 200, body: { target: "copy" } },
      { status: 200, body: { target: "branch", branched_from: "ns" } },
      { status: 200, body: { segments_loaded: 3 } },
      { status: 204 },
      { status: 204 },
    ]);
    const ns = client.namespace("ns");
    await ns.copyTo("copy");
    await ns.branchTo("branch");
    await ns.warm();
    await ns.pin();
    await ns.unpin();
    expect(mock.requests[0]!.url).toContain("/v1/namespaces/ns/copy");
    expect(mock.requests[1]!.url).toContain("/v1/namespaces/ns/branch");
    expect(mock.requests[2]!.url).toContain("/v1/namespaces/ns/warm");
    expect(mock.requests[3]!.body).toEqual({ pinned: true });
    expect(mock.requests[4]!.body).toEqual({ pinned: false });
  });

  it("checks health and fetches metrics text", async () => {
    const mock = mockFetch([
      { status: 200, body: { status: "ok", version: "0.5.0" } },
    ]);
    const client = new ShoalClient({
      baseUrl: "http://test.local",
      fetch: mock.fetch,
      sleep: instantSleep,
    });
    const health = await client.health();
    expect(health.status).toBe("ok");
    expect(mock.requests[0]!.url).toBe("http://test.local/healthz");
  });
});
