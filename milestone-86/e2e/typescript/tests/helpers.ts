/**
 * Shared data and helpers for the TypeScript live-server e2e suite.
 *
 * Mirrors e2e/python/helpers.py so both SDKs are held to the same contract.
 */
import { ShoalClient } from "@sdk";

export const E2E_URL = (process.env.SHOAL_E2E_URL ?? "").replace(/\/+$/, "");
export const E2E_API_KEY = process.env.SHOAL_E2E_API_KEY ?? "dev-root-key";

/** The suite is opt-in: skip everything unless a target URL is configured. */
export const enabled = E2E_URL.length > 0;

export const NATURE_QUERY = [1.0, 0.0, 0.0, 0.0];
export const TECH_QUERY = [0.0, 1.0, 0.0, 0.0];

export interface CorpusDoc {
  id: string;
  vector: number[];
  attributes: Record<string, unknown>;
}

export const CORPUS: CorpusDoc[] = [
  {
    id: "doc-1",
    vector: [1.0, 0.0, 0.0, 0.0],
    attributes: {
      title: "Coral reef ecosystems",
      body: "Coral reefs host shoals of vibrant fish in warm shallow ocean water.",
      genre: "nature",
      year: 2019,
      rating: 4.5,
      tags: ["ocean", "fish"],
    },
  },
  {
    id: "doc-2",
    vector: [0.9, 0.1, 0.0, 0.0],
    attributes: {
      title: "Mangrove forests",
      body: "Mangroves shelter coastal nurseries where juvenile fish grow.",
      genre: "nature",
      year: 2021,
      rating: 4.2,
      tags: ["ocean", "trees"],
    },
  },
  {
    id: "doc-3",
    vector: [0.0, 1.0, 0.0, 0.0],
    attributes: {
      title: "Vector databases explained",
      body: "Approximate nearest neighbour indexes make embedding search fast.",
      genre: "tech",
      year: 2023,
      rating: 4.8,
      tags: ["search", "ai"],
    },
  },
  {
    id: "doc-4",
    vector: [0.1, 0.9, 0.0, 0.0],
    attributes: {
      title: "BM25 ranking deep dive",
      body: "An inverted index with term frequency saturation powers lexical ranking.",
      genre: "tech",
      year: 2022,
      rating: 4.0,
      tags: ["search", "text"],
    },
  },
  {
    id: "doc-5",
    vector: [0.0, 0.0, 1.0, 0.0],
    attributes: {
      title: "Sourdough basics",
      body: "Flour, water, and a lively starter ferment into rustic bread.",
      genre: "cooking",
      year: 2020,
      rating: 3.9,
      tags: ["bread"],
    },
  },
  {
    id: "doc-6",
    vector: [0.0, 0.1, 0.9, 0.0],
    attributes: {
      title: "Ramen broth",
      body: "Simmer bones for hours to extract deep umami for the noodles.",
      genre: "cooking",
      year: 2018,
      rating: 4.6,
      tags: ["soup"],
    },
  },
  {
    id: "doc-7",
    vector: [0.0, 0.0, 0.0, 1.0],
    attributes: {
      title: "The silk road",
      body: "Caravans carried silk and ideas along ancient trade routes across Asia.",
      genre: "history",
      year: 2015,
      rating: 4.1,
      tags: ["trade"],
    },
  },
  {
    id: "doc-8",
    vector: [0.1, 0.0, 0.0, 0.9],
    attributes: {
      title: "Age of sail",
      body: "Tall ships crossed the ocean guided by stars and sextants.",
      genre: "history",
      year: 2016,
      rating: 3.7,
      tags: ["ocean", "ships"],
    },
  },
];

export const CORPUS_IDS = CORPUS.map((d) => d.id);

export function makeClient(): ShoalClient {
  return new ShoalClient({ baseUrl: E2E_URL, apiKey: E2E_API_KEY });
}

export function uniqueNs(prefix = "e2e-ts"): string {
  return `${prefix}-${globalThis.crypto.randomUUID().replace(/-/g, "").slice(0, 10)}`;
}

export async function waitForHealthy(timeoutMs = 180_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastErr = "no attempt made";
  while (Date.now() < deadline) {
    try {
      const resp = await fetch(`${E2E_URL}/healthz`);
      if (resp.ok) return;
      lastErr = `/healthz returned HTTP ${resp.status}`;
    } catch (err) {
      lastErr = String(err);
    }
    await sleep(1000);
  }
  throw new Error(`server at ${E2E_URL} never became healthy: ${lastErr}`);
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Retry an assertion-throwing check until it passes or the timeout elapses.
 * Keeps ranking-sensitive assertions robust against brief background-index
 * lag without masking real failures.
 */
export async function eventually(
  check: () => Promise<void> | void,
  timeoutMs = 30_000,
  intervalMs = 500,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  // eslint-disable-next-line no-constant-condition
  while (true) {
    try {
      await check();
      return;
    } catch (err) {
      if (Date.now() >= deadline) throw err;
      await sleep(intervalMs);
    }
  }
}

// ---------------------------------------------------------------------------
// Shape-tolerant accessors (behaviour over representation)
// ---------------------------------------------------------------------------

type AnyRow = { id: string; attributes?: Record<string, unknown> };

export function resultRows(res: unknown): AnyRow[] {
  const maybe = res as { results?: AnyRow[] };
  if (Array.isArray(maybe.results)) return maybe.results;
  if (Array.isArray(res)) return res as AnyRow[];
  throw new Error(`unexpected query result shape: ${JSON.stringify(res)}`);
}

export function resultIds(res: unknown): string[] {
  return resultRows(res).map((r) => r.id);
}

export function resultAttrs(res: unknown): Record<string, unknown>[] {
  return resultRows(res).map((r) => r.attributes ?? {});
}

/** Collect exported documents regardless of array vs. async-iterable shape. */
export async function exportRows(ns: {
  export: () => unknown;
}): Promise<AnyRow[]> {
  const exported = await Promise.resolve(ns.export());
  const out: AnyRow[] = [];
  if (exported && typeof exported === "object" && Symbol.asyncIterator in exported) {
    for await (const doc of exported as AsyncIterable<AnyRow>) out.push(doc);
    return out;
  }
  if (Array.isArray(exported)) return exported as AnyRow[];
  const wrapped = exported as { documents?: AnyRow[] };
  if (Array.isArray(wrapped.documents)) return wrapped.documents;
  throw new Error(`unexpected export shape: ${JSON.stringify(exported)}`);
}

export async function exportIds(ns: { export: () => unknown }): Promise<string[]> {
  return (await exportRows(ns)).map((d) => d.id);
}

export async function exportAttrsById(ns: {
  export: () => unknown;
}): Promise<Record<string, Record<string, unknown>>> {
  const out: Record<string, Record<string, unknown>> = {};
  for (const row of await exportRows(ns)) out[row.id] = row.attributes ?? {};
  return out;
}
