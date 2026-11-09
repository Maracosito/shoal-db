/**
 * Wire-level types for the Shoal HTTP API.
 *
 * Field names intentionally match the JSON wire format (snake_case) so that
 * what you write in TypeScript is exactly what is sent — no hidden renaming.
 */

import type { Filter } from "./filters.js";

/** Sparse vector: parallel arrays of dimension indices and values. */
export interface SparseVector {
  indices: number[];
  values: number[];
}

/** A document stored in a namespace. */
export interface Document {
  id: string;
  /** Dense embedding. Length must match the namespace's vector dimension. */
  vector?: number[];
  /** Optional sparse vector (e.g. SPLADE / BM25-weighted terms). */
  sparse_vector?: SparseVector;
  /** Full-text fields, keyed by field name. */
  text?: Record<string, string>;
  /** Arbitrary metadata attributes, filterable. */
  attributes?: Record<string, unknown>;
}

/** Partial update of a document: only present fields are modified. */
export interface DocumentPatch {
  id: string;
  vector?: number[];
  sparse_vector?: SparseVector;
  text?: Record<string, string>;
  attributes?: Record<string, unknown>;
}

export type DistanceMetric = "cosine" | "dot" | "euclidean";

/** Configuration supplied at namespace creation time. */
export interface NamespaceConfig {
  /** Dense vector dimensionality. Omit for text-only namespaces. */
  vector_dims?: number;
  /** Default distance metric. */
  metric?: DistanceMetric;
  /** Full-text fields with default boost weights, e.g. {"title": 2.0, "body": 1.0}. */
  text_fields?: Record<string, number>;
  /** Attribute names to build filter indexes for. */
  indexed_attributes?: string[];
}

/** Server-reported statistics for a namespace. */
export interface NamespaceStats {
  document_count?: number;
  segment_count?: number;
  wal_bytes?: number;
  index_bytes?: number;
  total_bytes?: number;
  [key: string]: unknown;
}

/** Namespace metadata as returned by the API. */
export interface NamespaceInfo {
  name: string;
  config?: NamespaceConfig;
  created_at?: string;
  updated_at?: string;
  /** Present when the namespace is a branch of another namespace. */
  branched_from?: string;
  pinned?: boolean;
  stats?: NamespaceStats;
  [key: string]: unknown;
}

export interface ListNamespacesResponse {
  namespaces: NamespaceInfo[];
  next_cursor?: string | null;
}

/** Optional conditional-write predicate for upserts. */
export interface WriteCondition {
  /** Only write documents whose IDs do not already exist. */
  if_absent?: boolean;
  /** Only apply if the namespace WAL version matches (optimistic concurrency). */
  if_version?: number;
}

/** Row-oriented upsert request. */
export interface UpsertRequest {
  documents: Document[];
  condition?: WriteCondition;
}

/** Column-oriented upsert request. Arrays must all share the same length. */
export interface ColumnarUpsert {
  ids: string[];
  vectors?: number[][];
  sparse_vectors?: SparseVector[];
  /** Per-field arrays of text values. */
  text?: Record<string, (string | null)[]>;
  /** Per-attribute arrays of values. */
  attributes?: Record<string, unknown[]>;
}

export interface UpsertResponse {
  upserted: number;
  /** WAL sequence number assigned to this write batch. */
  wal_sequence?: number;
  [key: string]: unknown;
}

export interface PatchResponse {
  patched: number;
  wal_sequence?: number;
  [key: string]: unknown;
}

export interface DeleteResponse {
  deleted: number;
  wal_sequence?: number;
  [key: string]: unknown;
}

/** Score fusion configuration for hybrid queries. */
export interface FusionConfig {
  /** "rrf" = reciprocal rank fusion, "weighted" = weighted score sum. */
  method: "rrf" | "weighted";
  /** RRF constant k (default 60). Only used when method = "rrf". */
  rrf_k?: number;
  /** Weight applied to vector scores. Only used when method = "weighted". */
  vector_weight?: number;
  /** Weight applied to text scores. Only used when method = "weighted". */
  text_weight?: number;
}

export type QueryMode = "auto" | "vector" | "text" | "sparse" | "hybrid";

/** A single query. Provide `vector`, `text`, or both (hybrid). */
export interface QueryRequest {
  vector?: number[];
  sparse_vector?: SparseVector;
  /** Full-text query string (BM25 ranked). */
  text?: string;
  /** Per-field boosts for the text query, overriding namespace defaults. */
  text_fields?: Record<string, number>;
  filter?: Filter;
  top_k?: number;
  metric?: DistanceMetric;
  /** Execution mode hint; "auto" lets the planner decide. */
  mode?: QueryMode;
  fusion?: FusionConfig;
  /** true = all attributes, false = none, or an explicit list of names. */
  include_attributes?: boolean | string[];
  include_vectors?: boolean;
  /** Force exact kNN instead of the ANN index. */
  exact?: boolean;
  /** "strong" forces a WAL tail read for read-your-writes; "eventual" may serve from index only. */
  consistency?: "strong" | "eventual";
}

export interface QueryResult {
  id: string;
  score: number;
  vector?: number[];
  text?: Record<string, string>;
  attributes?: Record<string, unknown>;
  [key: string]: unknown;
}

export interface QueryResponse {
  results: QueryResult[];
  took_ms?: number;
  /** The execution plan chosen by the planner, e.g. "ann+filter" or "hybrid_rrf". */
  plan?: string;
  cache?: "cold" | "warm" | "hot";
  [key: string]: unknown;
}

export interface MultiQueryResponse {
  responses: QueryResponse[];
  took_ms?: number;
}

export interface ExportPage {
  documents: Document[];
  next_cursor?: string | null;
}

export interface CopyNamespaceResponse {
  target: string;
  [key: string]: unknown;
}

export interface BranchNamespaceResponse {
  target: string;
  branched_from: string;
  [key: string]: unknown;
}

export interface WarmResponse {
  segments_loaded?: number;
  bytes_loaded?: number;
  [key: string]: unknown;
}

export interface HealthStatus {
  status: string;
  version?: string;
  [key: string]: unknown;
}
