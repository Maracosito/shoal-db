/**
 * Error hierarchy for the Shoal TypeScript SDK.
 *
 * Every error thrown by the SDK is a subclass of {@link ShoalError}, so a
 * single `instanceof ShoalError` check covers all SDK failures. API failures
 * are mapped onto status-specific subclasses of {@link ApiError}.
 */

/** Base class for all errors raised by the SDK. */
export class ShoalError extends Error {
  constructor(message: string, options?: { cause?: unknown }) {
    super(message);
    this.name = "ShoalError";
    if (options?.cause !== undefined) {
      // Defined here for runtimes where Error options are not yet supported.
      (this as { cause?: unknown }).cause = options.cause;
    }
  }
}

/** A request could not be sent or the connection failed mid-flight. */
export class NetworkError extends ShoalError {
  constructor(message: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = "NetworkError";
  }
}

/** The request exceeded the configured timeout (or was aborted). */
export class TimeoutError extends ShoalError {
  constructor(message: string) {
    super(message);
    this.name = "TimeoutError";
  }
}

/** All retries were exhausted without a successful response. */
export class RetriesExhaustedError extends ShoalError {
  /** The last error encountered before giving up. */
  readonly lastError: ShoalError;
  readonly attempts: number;

  constructor(attempts: number, lastError: ShoalError) {
    super(
      `request failed after ${attempts} attempt(s); last error: ${lastError.message}`,
      { cause: lastError },
    );
    this.name = "RetriesExhaustedError";
    this.attempts = attempts;
    this.lastError = lastError;
  }
}

/** Structured error body returned by the Shoal API. */
export interface ApiErrorBody {
  error?: {
    code?: string;
    message?: string;
    details?: unknown;
  };
  [key: string]: unknown;
}

/** The server returned a non-2xx response. */
export class ApiError extends ShoalError {
  readonly status: number;
  readonly code?: string;
  readonly requestId?: string;
  readonly details?: unknown;

  constructor(
    message: string,
    status: number,
    opts?: { code?: string; requestId?: string; details?: unknown },
  ) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = opts?.code;
    this.requestId = opts?.requestId;
    this.details = opts?.details;
  }
}

/** 400 / 422 — the request payload was rejected by the server. */
export class ValidationError extends ApiError {
  constructor(message: string, status: number, opts?: ConstructorParameters<typeof ApiError>[2]) {
    super(message, status, opts);
    this.name = "ValidationError";
  }
}

/** 401 / 403 — missing, invalid, or insufficiently privileged API key. */
export class AuthenticationError extends ApiError {
  constructor(message: string, status: number, opts?: ConstructorParameters<typeof ApiError>[2]) {
    super(message, status, opts);
    this.name = "AuthenticationError";
  }
}

/** 404 — namespace or document not found. */
export class NotFoundError extends ApiError {
  constructor(message: string, status: number, opts?: ConstructorParameters<typeof ApiError>[2]) {
    super(message, status, opts);
    this.name = "NotFoundError";
  }
}

/** 409 — conflicting write (e.g. failed conditional write, namespace exists). */
export class ConflictError extends ApiError {
  constructor(message: string, status: number, opts?: ConstructorParameters<typeof ApiError>[2]) {
    super(message, status, opts);
    this.name = "ConflictError";
  }
}

/** 429 — rate limited. `retryAfterMs` is populated from the Retry-After header. */
export class RateLimitError extends ApiError {
  readonly retryAfterMs?: number;

  constructor(
    message: string,
    status: number,
    opts?: ConstructorParameters<typeof ApiError>[2] & { retryAfterMs?: number },
  ) {
    super(message, status, opts);
    this.name = "RateLimitError";
    this.retryAfterMs = opts?.retryAfterMs;
  }
}

/** 5xx — server-side failure. Retried automatically by the transport. */
export class ServerError extends ApiError {
  constructor(message: string, status: number, opts?: ConstructorParameters<typeof ApiError>[2]) {
    super(message, status, opts);
    this.name = "ServerError";
  }
}

/** Parse a Retry-After header value into milliseconds (seconds or HTTP date). */
export function parseRetryAfterMs(value: string | null): number | undefined {
  if (!value) return undefined;
  const seconds = Number(value);
  if (Number.isFinite(seconds)) {
    return Math.max(0, seconds * 1000);
  }
  const date = Date.parse(value);
  if (!Number.isNaN(date)) {
    return Math.max(0, date - Date.now());
  }
  return undefined;
}

/**
 * Map an HTTP status + parsed error body onto the appropriate ApiError subclass.
 */
export function errorFromResponse(
  status: number,
  body: ApiErrorBody | undefined,
  headers: Headers,
): ApiError {
  const code = body?.error?.code;
  const details = body?.error?.details;
  const requestId = headers.get("x-request-id") ?? undefined;
  const message =
    body?.error?.message ?? `request failed with status ${status}`;
  const opts = { code, requestId, details };

  if (status === 400 || status === 422) return new ValidationError(message, status, opts);
  if (status === 401 || status === 403) return new AuthenticationError(message, status, opts);
  if (status === 404) return new NotFoundError(message, status, opts);
  if (status === 409) return new ConflictError(message, status, opts);
  if (status === 429) {
    return new RateLimitError(message, status, {
      ...opts,
      retryAfterMs: parseRetryAfterMs(headers.get("retry-after")),
    });
  }
  if (status >= 500) return new ServerError(message, status, opts);
  return new ApiError(message, status, opts);
}
