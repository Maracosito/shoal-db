/**
 * Top-level Shoal client.
 *
 * ```ts
 * import { ShoalClient } from "@shoal-db/client";
 *
 * const client = new ShoalClient({
 *   baseUrl: "http://localhost:8800",
 *   apiKey: process.env.SHOAL_API_KEY,
 * });
 *
 * const ns = await client.createNamespace("articles", {
 *   vector_dims: 384,
 *   metric: "cosine",
 *   text_fields: { title: 2.0, body: 1.0 },
 * });
 * ```
 */

import { Namespace } from "./namespace.js";
import { Transport, type TransportOptions } from "./transport.js";
import type {
  HealthStatus,
  ListNamespacesResponse,
  NamespaceConfig,
  NamespaceInfo,
} from "./types.js";

export interface ShoalClientOptions extends Omit<TransportOptions, "baseUrl"> {
  /** Base URL of the Shoal API server. Default "http://localhost:8800". */
  baseUrl?: string;
}

export interface ListNamespacesOptions {
  /** Filter namespaces by name prefix. */
  prefix?: string;
  /** Page size per request. Default 100. */
  pageSize?: number;
}

export class ShoalClient {
  private readonly transport: Transport;

  constructor(options: ShoalClientOptions = {}) {
    this.transport = new Transport({
      ...options,
      baseUrl: options.baseUrl ?? "http://localhost:8800",
    });
  }

  /** Get a handle on a namespace. Does not call the server. */
  namespace(name: string): Namespace {
    return new Namespace(this.transport, name);
  }

  /** Create a namespace. Fails with ConflictError if it already exists. */
  async createNamespace(name: string, config?: NamespaceConfig): Promise<NamespaceInfo> {
    return this.transport.request<NamespaceInfo>(
      "PUT",
      `/v1/namespaces/${encodeURIComponent(name)}`,
      { body: { config: config ?? {} } },
    );
  }

  /** Delete a namespace by name. */
  async deleteNamespace(name: string): Promise<void> {
    await this.namespace(name).delete();
  }

  /** List all namespaces, transparently following pagination. */
  async listNamespaces(opts: ListNamespacesOptions = {}): Promise<NamespaceInfo[]> {
    const out: NamespaceInfo[] = [];
    let cursor: string | undefined;
    while (true) {
      const page = await this.transport.request<ListNamespacesResponse>(
        "GET",
        "/v1/namespaces",
        {
          query: {
            prefix: opts.prefix,
            limit: opts.pageSize ?? 100,
            cursor,
          },
        },
      );
      out.push(...page.namespaces);
      if (!page.next_cursor) {
        return out;
      }
      cursor = page.next_cursor;
    }
  }

  /** Server health check. */
  async health(): Promise<HealthStatus> {
    return this.transport.request<HealthStatus>("GET", "/healthz");
  }

  /** Raw Prometheus metrics exposition text. */
  async metrics(): Promise<string> {
    return this.transport.requestText("GET", "/metrics");
  }
}
