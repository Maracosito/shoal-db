import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { F, type ShoalClient } from "@sdk";
import {
  CORPUS,
  CORPUS_IDS,
  NATURE_QUERY,
  TECH_QUERY,
  enabled,
  eventually,
  exportAttrsById,
  exportIds,
  makeClient,
  resultAttrs,
  resultIds,
  uniqueNs,
  waitForHealthy,
} from "./helpers";

describe.skipIf(!enabled)("query behaviour (TypeScript SDK)", () => {
  let client: ShoalClient;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let ns: any;
  const created: string[] = [];

  beforeAll(async () => {
    await waitForHealthy();
    client = makeClient();
    const name = uniqueNs("tsq");
    created.push(name);
    await client.createNamespace(name);
    ns = client.namespace(name);
    await ns.upsert(CORPUS);
  });

  afterAll(async () => {
    for (const name of created.reverse()) {
      try {
        await client.deleteNamespace(name);
      } catch {
        /* best-effort cleanup */
      }
    }
  });

  it("vector search returns the nearest documents first", async () => {
    await eventually(async () => {
      const res = await ns.query({ vector: NATURE_QUERY, topK: 3 });
      const ids = resultIds(res);
      expect(ids).toHaveLength(3);
      expect(ids[0]).toBe("doc-1");
      expect(ids[1]).toBe("doc-2");
    });
  });

  it("full-text search ranks lexical matches first", async () => {
    await eventually(async () => {
      const res = await ns.query({ text: "coral reef fish ocean", topK: 5 });
      const ids = resultIds(res);
      expect(ids.length).toBeGreaterThan(0);
      expect(ids[0]).toBe("doc-1");
    });
  });

  it("hybrid search combines both signals", async () => {
    await eventually(async () => {
      const res = await ns.query({
        vector: TECH_QUERY,
        text: "inverted index ranking",
        topK: 4,
      });
      const ids = resultIds(res);
      expect(ids).toContain("doc-3");
      expect(ids).toContain("doc-4");
      expect(["doc-3", "doc-4"]).toContain(ids[0]);
    });
  });

  it("respects topK", async () => {
    for (const k of [1, 2, 5]) {
      const res = await ns.query({ vector: NATURE_QUERY, topK: k });
      expect(resultIds(res)).toHaveLength(k);
    }
  });

  it("filter operators select the right documents", async () => {
    const cases: Array<[unknown, string[]]> = [
      [F.eq("genre", "nature"), ["doc-1", "doc-2"]],
      [F.gte("year", 2020), ["doc-2", "doc-3", "doc-4", "doc-5"]],
      [F.in_("genre", ["nature", "history"]), ["doc-1", "doc-2", "doc-7", "doc-8"]],
      [F.containsAny("tags", ["ocean", "soup"]), ["doc-1", "doc-2", "doc-6", "doc-8"]],
      [F.eq("genre", "tech").and(F.gte("year", 2023)), ["doc-3"]],
      [
        F.eq("genre", "history").or(F.gte("rating", 4.6)),
        ["doc-3", "doc-6", "doc-7", "doc-8"],
      ],
      [
        F.eq("genre", "nature").not(),
        ["doc-3", "doc-4", "doc-5", "doc-6", "doc-7", "doc-8"],
      ],
    ];
    for (const [filter, expected] of cases) {
      await eventually(async () => {
        const res = await ns.query({ filter, topK: CORPUS.length + 5 });
        expect(resultIds(res).sort()).toEqual([...expected].sort());
      });
    }
  });

  it("includeAttributes projects only the requested fields", async () => {
    await eventually(async () => {
      const res = await ns.query({
        vector: NATURE_QUERY,
        topK: 1,
        includeAttributes: ["title"],
      });
      const attrs = resultAttrs(res)[0];
      expect(attrs).toHaveProperty("title");
      expect(attrs).not.toHaveProperty("body");
    });
  });
});

describe.skipIf(!enabled)("mutations (TypeScript SDK)", () => {
  let client: ShoalClient;
  const created: string[] = [];

  beforeAll(async () => {
    await waitForHealthy();
    client = makeClient();
  });

  afterAll(async () => {
    for (const name of created.reverse()) {
      try {
        await client.deleteNamespace(name);
      } catch {
        /* best-effort cleanup */
      }
    }
  });

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  async function seeded(): Promise<any> {
    const name = uniqueNs("tsm");
    created.push(name);
    await client.createNamespace(name);
    const ns = client.namespace(name);
    await ns.upsert(CORPUS);
    return ns;
  }

  it("upsert is idempotent", async () => {
    const ns = await seeded();
    await ns.upsert(CORPUS);
    expect((await exportIds(ns)).sort()).toEqual([...CORPUS_IDS].sort());
  });

  it("patch updates attributes without clobbering others", async () => {
    const ns = await seeded();
    await ns.patch([{ id: "doc-1", attributes: { genre: "updated" } }]);
    const byId = await exportAttrsById(ns);
    expect(byId["doc-1"].genre).toBe("updated");
    expect(byId["doc-1"].title).toBe("Coral reef ecosystems");
    expect(byId["doc-2"].genre).toBe("nature");
  });

  it("deletes by id", async () => {
    const ns = await seeded();
    await ns.delete(["doc-1", "doc-2"]);
    const ids = await exportIds(ns);
    expect(ids).not.toContain("doc-1");
    expect(ids).not.toContain("doc-2");
    expect(ids).toHaveLength(CORPUS.length - 2);
  });

  it("deletes by filter", async () => {
    const ns = await seeded();
    await ns.deleteByFilter(F.eq("genre", "history"));
    const ids = (await exportIds(ns)).sort();
    expect(ids).toEqual(
      ["doc-1", "doc-2", "doc-3", "doc-4", "doc-5", "doc-6"].sort(),
    );
  });

  it("deleted documents disappear from query results", async () => {
    const ns = await seeded();
    await ns.delete(["doc-1"]);
    await eventually(async () => {
      const res = await ns.query({ vector: NATURE_QUERY, topK: CORPUS.length });
      expect(resultIds(res)).not.toContain("doc-1");
    });
  });
});
