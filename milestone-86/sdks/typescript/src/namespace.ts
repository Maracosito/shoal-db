/**
 * Namespace handle: all document- and namespace-scoped operations.
 */

import {
  chunkDocuments,
  mapWithConcurrency,
  type BatchOptions,
  type BatchProgress,
} from "./batch.js";
import type { Filter } from "./filters.js";
import type { Transport } from "./transport.js";
import type {
  BranchNamespaceResponse,
  ColumnarUpsert,
  CopyNamespaceResponse,
  DeleteResponse,
  Document,
  DocumentPatch,
  ExportPage,
  MultiQueryResponse,
  NamespaceConfig,
  NamespaceInfo,
  PatchResponse,
  QueryRequest,
  QueryResponse,
  UpsertResponse,
  WarmResponse,
  WriteCondition,
} from "./types.js";

export interface UpsertOptions {
  condition?: WriteCondition;
  idempotencyKey?: string;
}

export interface ExportOptions {
  /** Documents per page. Default 1000. */
  pageSize?: number;
  /** Resume from a previous cursor. */
  cursor?: string;
}

export interface WarmOptions {
  /** Warm only specific segment IDs; omit to warm the whole namespace. */
  segments?: string[];
}

function encodeName(name: string): string {
  return encodeURIComponent(name);
}

export class Namespace {
  readonly name: string;
  private readonly transport: Transport;
  private readonly path: string;

  constructor(transport: Transport, name: string) {
    if (!name) {
      throw new Error("namespace name must not be empty");
    }
    this.transport = transport;
    this.name = name;
    this.path = `/v1/namespaces/${encodeName(name)}`;
  }

  // ---------------------------------------------------------------- metadata

  /** Fetch namespace metadata and stats. */
  async info(): Promise<NamespaceInfo> {
    return this.transport.request<NamespaceInfo>("GET", this.path);
  }

  /** Update namespace configuration (only mutable settings). */
  async updateConfig(config: Partial<NamespaceConfig>): Promise<NamespaceInfo> {
    return this.transport.request<NamespaceInfo>("PATCH", this.path, {
      body: { config },
    });
  }

  /** Delete this namespace. Shared branch data is reference-counted server-side. */
  async delete(): Promise<void> {
    await this.transport.request<void>("DELETE", this.path);
  }

  // ------------------------------------------------------------------ writes

  /** Upsert documents (row-oriented). For large sets prefer {@link upsertBatched}. */
  async upsert(documents: Document[], opts: UpsertOptions = {}): Promise<UpsertResponse> {
    return this.transport.request<UpsertResponse>("POST", `${this.path}/documents`, {
      body: { documents, condition: opts.condition },
      idempotencyKey: opts.idempotencyKey,
    });
  }

  /** Upsert documents in column-oriented form (efficient for bulk loads). */
  async upsertColumns(
    columns: ColumnarUpsert,
    opts: UpsertOptions = {},
  ): Promise<UpsertResponse> {
    return this.transport.request<UpsertResponse>("POST", `${this.path}/documents`, {
      body: { columns, condition: opts.condition },
      idempotencyKey: opts.idempotencyKey,
    });
  }

  /**
   * Upsert an arbitrarily large document set in size-bounded batches with
   * limited concurrency. Returns the aggregate count and per-batch responses.
   */
  async upsertBatched(
    documents: Document[],
    opts: BatchOptions & { condition?: WriteCondition } = {},
  ): Promise<{ upserted: number; batches: UpsertResponse[] }> {
    const batchSize = opts.batchSize ?? 500;
    const maxBatchBytes = opts.maxBatchBytes ?? 8 * 1024 * 1024;
    const concurrency = opts.concurrency ?? 2;
    const batches = chunkDocuments(documents, batchSize, maxBatchBytes);

    let batchesDone = 0;
    let documentsDone = 0;
    const reportProgress = (batchLen: number) => {
      batchesDone += 1;
      documentsDone += batchLen;
      const progress: BatchProgress = {
        batchesDone,
        batchesTotal: batches.length,
        documentsDone,
        documentsTotal: documents.length,
      };
      opts.onProgress?.(progress);
    };

    const responses = await mapWithConcurrency(
      batches,
      concurrency,
      async (batch, index) => {
        const idempotencyKey = opts.idempotencyPrefix
          ? `${opts.idempotencyPrefix}:${index}`
          : undefined;
        const res = await this.upsert(batch, {
          condition: opts.condition,
          idempotencyKey,
        });
        reportProgress(batch.length);
        return res;
      },
    );

    const upserted = responses.reduce((sum, r) => sum + (r.upserted ?? 0), 0);
    return { upserted, batches: responses };
  }

  /** Partially update documents: only the provided fields change. */
  async patch(patches: DocumentPatch[], opts: UpsertOptions = {}): Promise<PatchResponse> {
    return this.transport.request<PatchResponse>("PATCH", `${this.path}/documents`, {
      body: { documents: patches },
      idempotencyKey: opts.idempotencyKey,
    });
  }

  /** Delete documents by ID. */
  async deleteByIds(ids: string[], opts: { idempotencyKey?: string } = {}): Promise<DeleteResponse> {
    return this.transport.request<DeleteResponse>("POST", `${this.path}/documents/delete`, {
      body: { ids },
      idempotencyKey: opts.idempotencyKey,
    });
  }

  /** Delete all documents matching a filter expression. */
  async deleteByFilter(
    filter: Filter,
    opts: { idempotencyKey?: string } = {},
  ): Promise<DeleteResponse> {
    return this.transport.request<DeleteResponse>("POST", `${this.path}/documents/delete`, {
      body: { filter },
      idempotencyKey: opts.idempotencyKey,
    });
  }

  // ----------------------------------------------------------------- queries

  /** Run a single query (vector, text, sparse, or hybrid). */
  async query(request: QueryRequest): Promise<QueryResponse> {
    return this.transport.request<QueryResponse>("POST", `${this.path}/query`, {
      body: request,
    });
  }

  /** Run multiple queries in a single round trip. */
  async multiQuery(requests: QueryRequest[]): Promise<MultiQueryResponse> {
    return this.transport.request<MultiQueryResponse>("POST", `${this.path}/query`, {
      body: { queries: requests },
    });
  }

  // ------------------------------------------------------------------ export

  /** Fetch a single export page. */
  async exportPage(opts: ExportOptions = {}): Promise<ExportPage> {
    return this.transport.request<ExportPage>("GET", `${this.path}/export`, {
      query: {
        limit: opts.pageSize ?? 1000,
        cursor: opts.cursor,
      },
    });
  }

  /**
   * Iterate every document in the namespace, transparently following
   * pagination cursors:
   *
   * ```ts
   * for await (const doc of ns.exportDocuments()) { ... }
   * ```
   */
  async *exportDocuments(opts: ExportOptions = {}): AsyncGenerator<Document, void, void> {
    let cursor = opts.cursor;
    while (true) {
      const page = await this.exportPage({ pageSize: opts.pageSize, cursor });
      for (const doc of page.documents) {
        yield doc;
      }
      if (!page.next_cursor) {
        return;
      }
      cursor = page.next_cursor;
    }
  }

  // -------------------------------------------------- copy / branch / warm

  /** Deep-copy this namespace into a new independent namespace. */
  async copyTo(target: string): Promise<CopyNamespaceResponse> {
    return this.transport.request<CopyNamespaceResponse>("POST", `${this.path}/copy`, {
      body: { target },
    });
  }

  /**
   * Create a copy-on-write branch of this namespace. The branch shares
   * immutable segments with the source until either side diverges.
   */
  async branchTo(target: string): Promise<BranchNamespaceResponse> {
    return this.transport.request<BranchNamespaceResponse>("POST", `${this.path}/branch`, {
      body: { target },
    });
  }

  /** Preload this namespace (or selected segments) into the node's caches. */
  async warm(opts: WarmOptions = {}): Promise<WarmResponse> {
    return this.transport.request<WarmResponse>("POST", `${this.path}/warm`, {
      body: { segments: opts.segments },
    });
  }

  /** Pin this namespace so it is never evicted from cache. */
  async pin(): Promise<void> {
    await this.transport.request<void>("POST", `${this.path}/pin`, {
      body: { pinned: true },
    });
  }

  /** Unpin this namespace. */
  async unpin(): Promise<void> {
    await this.transport.request<void>("POST", `${this.path}/pin`, {
      body: { pinned: false },
    });
  }
}
