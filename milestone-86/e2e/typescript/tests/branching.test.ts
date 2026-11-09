import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { ShoalClient } from "@sdk";
import {
  CORPUS,
  CORPUS_IDS,
  NATURE_QUERY,
  enabled,
  eventually,
  exportIds,
  makeClient,
  resultIds,
  uniqueNs,
  waitForHealthy,
} from "./helpers";

describe.skipIf(!enabled)("namespace branching (TypeScript SDK)", () => {
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
  async function seededSource(): Promise<[string, any]> {
    const name = uniqueNs("src");
    created.push(name);
    await client.createNamespace(name);
    const ns = client.namespace(name);
    await ns.upsert(CORPUS);
    return [name, ns];
  }

  const extraDoc = (id: string) => ({
    id,
    vector: [0.5, 0.5, 0, 0],
    attributes: { title: id, genre: "extra" },
  });

  it("a branch sees all source data immediately", async () => {
    const [srcName] = await seededSource();
    const branchName = uniqueNs("branch");
    created.push(branchName);
    await client.branchNamespace(srcName, branchName);
    const branch = client.namespace(branchName);

    expect((await exportIds(branch)).sort()).toEqual([...CORPUS_IDS].sort());
    await eventually(async () => {
      const res = await branch.query({ vector: NATURE_QUERY, topK: 1 });
      expect(resultIds(res)[0]).toBe("doc-1");
    });
  });

  it("branch writes do not leak to the source and vice versa", async () => {
    const [srcName, src] = await seededSource();
    const branchName = uniqueNs("branch");
    created.push(branchName);
    await client.branchNamespace(srcName, branchName);
    const branch = client.namespace(branchName);

    await branch.upsert([extraDoc("branch-only")]);
    await src.upsert([extraDoc("src-only")]);

    const srcIds = await exportIds(src);
    const branchIds = await exportIds(branch);

    expect(branchIds).toContain("branch-only");
    expect(srcIds).not.toContain("branch-only");
    expect(srcIds).toContain("src-only");
    expect(branchIds).not.toContain("src-only");
  });

  it("multi-level branches stay isolated", async () => {
    const [srcName, src] = await seededSource();

    const b1Name = uniqueNs("b1");
    created.push(b1Name);
    await client.branchNamespace(srcName, b1Name);
    const b1 = client.namespace(b1Name);
    await b1.upsert([extraDoc("level-1")]);

    const b2Name = uniqueNs("b2");
    created.push(b2Name);
    await client.branchNamespace(b1Name, b2Name);
    const b2 = client.namespace(b2Name);
    await b2.upsert([extraDoc("level-2")]);

    const srcIds = await exportIds(src);
    const b1Ids = await exportIds(b1);
    const b2Ids = await exportIds(b2);

    expect(srcIds).not.toContain("level-1");
    expect(srcIds).not.toContain("level-2");
    expect(b1Ids).toContain("level-1");
    expect(b1Ids).not.toContain("level-2");
    expect(b2Ids).toContain("level-1");
    expect(b2Ids).toContain("level-2");
  });

  it("deleting a branch preserves the shared source data", async () => {
    const [srcName, src] = await seededSource();
    const branchName = uniqueNs("branch");
    await client.branchNamespace(srcName, branchName);
    await client.deleteNamespace(branchName);

    expect((await exportIds(src)).sort()).toEqual([...CORPUS_IDS].sort());
    await eventually(async () => {
      const res = await src.query({ vector: NATURE_QUERY, topK: 1 });
      expect(resultIds(res)[0]).toBe("doc-1");
    });
  });

  it("a copy is fully independent of its source", async () => {
    const [srcName, src] = await seededSource();
    const copyName = uniqueNs("copy");
    created.push(copyName);
    await client.copyNamespace(srcName, copyName);
    const copy = client.namespace(copyName);

    expect((await exportIds(copy)).sort()).toEqual([...CORPUS_IDS].sort());

    await copy.delete(["doc-1"]);
    expect(await exportIds(copy)).not.toContain("doc-1");
    expect(await exportIds(src)).toContain("doc-1");
  });
});
