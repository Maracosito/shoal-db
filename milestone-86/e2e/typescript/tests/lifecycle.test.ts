import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { ShoalClient } from "@sdk";
import {
  enabled,
  exportIds,
  makeClient,
  uniqueNs,
  waitForHealthy,
} from "./helpers";

describe.skipIf(!enabled)("namespace lifecycle (TypeScript SDK)", () => {
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

  async function names(): Promise<string[]> {
    const listed = await client.listNamespaces();
    return (listed as unknown[]).map((n) =>
      typeof n === "string" ? n : (n as { name: string }).name,
    );
  }

  it("creates, lists, and deletes a namespace", async () => {
    const name = uniqueNs("lifecycle");
    created.push(name);
    await client.createNamespace(name);
    expect(await names()).toContain(name);

    await client.deleteNamespace(name);
    expect(await names()).not.toContain(name);
  });

  it("a freshly created namespace is empty and queryable", async () => {
    const name = uniqueNs("ryw");
    created.push(name);
    await client.createNamespace(name);
    const ns = client.namespace(name);
    expect(await exportIds(ns)).toEqual([]);
  });

  it("deleting a missing namespace rejects", async () => {
    await expect(
      client.deleteNamespace(uniqueNs("missing")),
    ).rejects.toThrow();
  });

  it("querying a missing namespace rejects", async () => {
    const ns = client.namespace(uniqueNs("missing"));
    await expect(
      ns.query({ vector: [1, 0, 0, 0], topK: 1 }),
    ).rejects.toThrow();
  });
});
