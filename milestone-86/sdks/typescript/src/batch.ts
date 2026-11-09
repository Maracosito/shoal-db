/**
 * Batching helpers: chunk large document sets into bounded batches and submit
 * them with limited concurrency, with progress callbacks and per-batch
 * idempotency keys for safe retries.
 */

import type { Document } from "./types.js";

export interface BatchProgress {
  /** Batches completed so far. */
  batchesDone: number;
  /** Total number of batches. */
  batchesTotal: number;
  /** Documents successfully submitted so far. */
  documentsDone: number;
  /** Total documents to submit. */
  documentsTotal: number;
}

export interface BatchOptions {
  /** Maximum documents per batch. Default 500. */
  batchSize?: number;
  /** Approximate maximum serialized bytes per batch. Default 8 MiB. */
  maxBatchBytes?: number;
  /** Number of batches in flight concurrently. Default 2. */
  concurrency?: number;
  /** Called after each batch completes. */
  onProgress?: (progress: BatchProgress) => void;
  /**
   * Prefix for generated per-batch idempotency keys. When set, each batch is
   * sent with `Idempotency-Key: <prefix>:<batchIndex>` so retries are safe.
   */
  idempotencyPrefix?: string;
}

/** Split documents into batches bounded by count and approximate byte size. */
export function chunkDocuments(
  documents: Document[],
  batchSize: number,
  maxBatchBytes: number,
): Document[][] {
  if (batchSize < 1) {
    throw new Error("batchSize must be >= 1");
  }
  const batches: Document[][] = [];
  let current: Document[] = [];
  let currentBytes = 0;

  for (const doc of documents) {
    const docBytes = approximateJsonBytes(doc);
    const wouldOverflow =
      current.length >= batchSize ||
      (current.length > 0 && currentBytes + docBytes > maxBatchBytes);
    if (wouldOverflow) {
      batches.push(current);
      current = [];
      currentBytes = 0;
    }
    current.push(doc);
    currentBytes += docBytes;
  }
  if (current.length > 0) {
    batches.push(current);
  }
  return batches;
}

/** Cheap serialized-size estimate without retaining the string. */
export function approximateJsonBytes(value: unknown): number {
  const s = JSON.stringify(value);
  return s === undefined ? 0 : s.length;
}

/**
 * Run an async function over items with bounded concurrency, preserving
 * result order. The first rejection aborts scheduling of further items and is
 * rethrown after in-flight items settle.
 */
export async function mapWithConcurrency<T, R>(
  items: T[],
  concurrency: number,
  fn: (item: T, index: number) => Promise<R>,
): Promise<R[]> {
  if (concurrency < 1) {
    throw new Error("concurrency must be >= 1");
  }
  const results: R[] = new Array(items.length);
  let nextIndex = 0;
  let firstError: unknown;
  let failed = false;

  async function worker(): Promise<void> {
    while (true) {
      if (failed) return;
      const index = nextIndex++;
      if (index >= items.length) return;
      try {
        results[index] = await fn(items[index] as T, index);
      } catch (err) {
        if (!failed) {
          failed = true;
          firstError = err;
        }
        return;
      }
    }
  }

  const workers: Promise<void>[] = [];
  const n = Math.min(concurrency, Math.max(items.length, 1));
  for (let i = 0; i < n; i++) {
    workers.push(worker());
  }
  await Promise.all(workers);

  if (failed) {
    throw firstError;
  }
  return results;
}
