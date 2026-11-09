import { describe, expect, it } from "vitest";
import {
  approximateJsonBytes,
  chunkDocuments,
  mapWithConcurrency,
} from "../src/batch.js";
import type { Document } from "../src/types.js";

function docs(n: number): Document[] {
  return Array.from({ length: n }, (_, i) => ({ id: `doc-${i}` }));
}

describe("chunkDocuments", () => {
  it("splits by batch size", () => {
    const batches = chunkDocuments(docs(10), 3, Number.MAX_SAFE_INTEGER);
    expect(batches.map((b) => b.length)).toEqual([3, 3, 3, 1]);
    expect(batches.flat().map((d) => d.id)).toEqual(docs(10).map((d) => d.id));
  });

  it("splits by byte budget", () => {
    const big: Document[] = Array.from({ length: 4 }, (_, i) => ({
      id: `d${i}`,
      attributes: { blob: "x".repeat(100) },
    }));
    const perDoc = approximateJsonBytes(big[0]);
    const batches = chunkDocuments(big, 100, perDoc * 2);
    expect(batches.length).toBe(2);
    expect(batches[0]!.length).toBe(2);
  });

  it("never emits an empty batch and keeps oversized docs alone", () => {
    const big: Document[] = [
      { id: "huge", attributes: { blob: "x".repeat(1000) } },
      { id: "small" },
    ];
    const batches = chunkDocuments(big, 10, 50);
    expect(batches.length).toBe(2);
    expect(batches[0]!.map((d) => d.id)).toEqual(["huge"]);
    expect(batches[1]!.map((d) => d.id)).toEqual(["small"]);
  });

  it("handles empty input", () => {
    expect(chunkDocuments([], 10, 1000)).toEqual([]);
  });
});

describe("mapWithConcurrency", () => {
  it("preserves order", async () => {
    const items = [30, 10, 20];
    const results = await mapWithConcurrency(items, 3, async (ms) => {
      await new Promise((r) => setTimeout(r, ms));
      return ms * 2;
    });
    expect(results).toEqual([60, 20, 40]);
  });

  it("bounds concurrency", async () => {
    let inFlight = 0;
    let peak = 0;
    await mapWithConcurrency(Array.from({ length: 10 }, (_, i) => i), 2, async () => {
      inFlight += 1;
      peak = Math.max(peak, inFlight);
      await new Promise((r) => setTimeout(r, 5));
      inFlight -= 1;
    });
    expect(peak).toBeLessThanOrEqual(2);
  });

  it("propagates the first error and stops scheduling", async () => {
    let started = 0;
    const err = await mapWithConcurrency(
      Array.from({ length: 10 }, (_, i) => i),
      1,
      async (i) => {
        started += 1;
        if (i === 2) throw new Error("boom");
      },
    ).catch((e) => e);
    expect(err).toBeInstanceOf(Error);
    expect((err as Error).message).toBe("boom");
    expect(started).toBe(3);
  });
});
