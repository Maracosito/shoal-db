export { ShoalClient } from "./client.js";
export type { ShoalClientOptions, ListNamespacesOptions } from "./client.js";

export { Namespace } from "./namespace.js";
export type { UpsertOptions, ExportOptions, WarmOptions } from "./namespace.js";

export { Transport } from "./transport.js";
export type { TransportOptions, RequestOptions, FetchLike } from "./transport.js";

export {
  ShoalError,
  ApiError,
  NetworkError,
  TimeoutError,
  RetriesExhaustedError,
  ValidationError,
  AuthenticationError,
  NotFoundError,
  ConflictError,
  RateLimitError,
  ServerError,
} from "./errors.js";

export {
  eq,
  neq,
  gt,
  gte,
  lt,
  lte,
  isIn,
  containsAny,
  prefix,
  and,
  or,
  not,
} from "./filters.js";
export type {
  Filter,
  Scalar,
  ComparisonFilter,
  InFilter,
  ContainsAnyFilter,
  PrefixFilter,
  AndFilter,
  OrFilter,
  NotFilter,
} from "./filters.js";

export {
  chunkDocuments,
  mapWithConcurrency,
  approximateJsonBytes,
} from "./batch.js";
export type { BatchOptions, BatchProgress } from "./batch.js";

export type {
  Document,
  DocumentPatch,
  SparseVector,
  DistanceMetric,
  NamespaceConfig,
  NamespaceInfo,
  NamespaceStats,
  ListNamespacesResponse,
  WriteCondition,
  UpsertRequest,
  ColumnarUpsert,
  UpsertResponse,
  PatchResponse,
  DeleteResponse,
  FusionConfig,
  QueryMode,
  QueryRequest,
  QueryResult,
  QueryResponse,
  MultiQueryResponse,
  ExportPage,
  CopyNamespaceResponse,
  BranchNamespaceResponse,
  WarmResponse,
  HealthStatus,
} from "./types.js";
