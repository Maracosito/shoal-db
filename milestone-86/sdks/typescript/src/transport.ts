/**
 * HTTP transport with timeouts, retries with exponential backoff + jitter,
 * Retry-After support, and structured error mapping.
 *
 * Uses the global `fetch` (Node >= 18, browsers, edge runtimes). A custom
 * fetch implementation can be injected for testing or instrumentation.
 */

import {
  ApiError,
  errorFromResponse,
  NetworkError,
  RateLimitError,
  RetriesExhaustedError,
  ServerError,
  ShoalError,
  TimeoutError,
  type ApiErrorBody,
} from "./errors.js";

export type FetchLike = (
  input: string | URL,
  init?: RequestInit,
) => Promise<Response>;

export interface TransportOptions {
  /** Base URL of the Shoal API server, e.g. "http://localhost:8800". */
  baseUrl: string;
  /** API key. Sent as `Authorization: Bearer <key>`. */
  apiKey?: string;
  /** Per-request timeout in milliseconds. Default 30000. */
  timeoutMs?: number;
  /** Maximum retry attempts after the first try. Default 3. */
  maxRetries?: number;
  /** Base backoff delay. Default 250 ms. */
  retryBaseDelayMs?: number;
  /** Backoff ceiling. Default 10000 ms. */
  retryMaxDelayMs?: number;
  /** Extra headers added to every request. */
  headers?: Record<string, string>;
  /** Custom fetch implementation (testing / instrumentation). */
  fetch?: FetchLike;
  /** Override the default User-Agent header. */
  userAgent?: string;
  /** Injectable sleep function (testing). */
  sleep?: (ms: number) => Promise<void>;
}

export interface RequestOptions {
  /** Query string parameters; undefined values are dropped. */
  query?: Record<string, string | number | boolean | undefined>;
  /** JSON body. */
  body?: unknown;
  /** Idempotency key; makes POST requests safely retryable. */
  idempotencyKey?: string;
  /** Per-call timeout override. */
  timeoutMs?: number;
  /** External abort signal. */
  signal?: AbortSignal;
}

const RETRYABLE_STATUSES = new Set([408, 425, 429, 500, 502, 503, 504]);
const IDEMPOTENT_METHODS = new Set(["GET", "HEAD", "PUT", "DELETE"]);

const SDK_VERSION = "0.5.0";

function defaultSleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export class Transport {
  private readonly baseUrl: string;
  private readonly apiKey?: string;
  private readonly timeoutMs: number;
  private readonly maxRetries: number;
  private readonly retryBaseDelayMs: number;
  private readonly retryMaxDelayMs: number;
  private readonly extraHeaders: Record<string, string>;
  private readonly fetchImpl: FetchLike;
  private readonly userAgent: string;
  private readonly sleep: (ms: number) => Promise<void>;

  constructor(options: TransportOptions) {
    if (!options.baseUrl) {
      throw new ShoalError("baseUrl is required");
    }
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.apiKey = options.apiKey;
    this.timeoutMs = options.timeoutMs ?? 30_000;
    this.maxRetries = options.maxRetries ?? 3;
    this.retryBaseDelayMs = options.retryBaseDelayMs ?? 250;
    this.retryMaxDelayMs = options.retryMaxDelayMs ?? 10_000;
    this.extraHeaders = options.headers ?? {};
    this.userAgent = options.userAgent ?? `shoal-ts/${SDK_VERSION}`;
    this.sleep = options.sleep ?? defaultSleep;

    const f = options.fetch ?? (globalThis as { fetch?: FetchLike }).fetch;
    if (!f) {
      throw new ShoalError(
        "global fetch is not available; pass a `fetch` implementation in TransportOptions (Node >= 18 required)",
      );
    }
    this.fetchImpl = f;
  }

  /** Perform a JSON request and decode the JSON response body as T. */
  async request<T>(
    method: string,
    path: string,
    opts: RequestOptions = {},
  ): Promise<T> {
    const response = await this.send(method, path, opts);
    if (response.status === 204) {
      return undefined as unknown as T;
    }
    const text = await response.text();
    if (text.length === 0) {
      return undefined as unknown as T;
    }
    try {
      return JSON.parse(text) as T;
    } catch (err) {
      throw new ShoalError(
        `failed to parse response body as JSON for ${method} ${path}`,
        { cause: err },
      );
    }
  }

  /** Perform a request and return the raw response text (e.g. /metrics). */
  async requestText(
    method: string,
    path: string,
    opts: RequestOptions = {},
  ): Promise<string> {
    const response = await this.send(method, path, opts);
    return response.text();
  }

  private buildUrl(path: string, query?: RequestOptions["query"]): string {
    const url = new URL(this.baseUrl + path);
    if (query) {
      for (const [key, value] of Object.entries(query)) {
        if (value !== undefined) {
          url.searchParams.set(key, String(value));
        }
      }
    }
    return url.toString();
  }

  private backoffDelay(attempt: number, retryAfterMs?: number): number {
    if (retryAfterMs !== undefined) {
      return Math.min(retryAfterMs, this.retryMaxDelayMs);
    }
    const exp = Math.min(
      this.retryMaxDelayMs,
      this.retryBaseDelayMs * Math.pow(2, attempt),
    );
    // Full jitter in [exp/2, exp].
    return exp / 2 + Math.random() * (exp / 2);
  }

  private isRetryable(method: string, opts: RequestOptions, status?: number): boolean {
    if (status !== undefined && !RETRYABLE_STATUSES.has(status)) {
      return false;
    }
    if (IDEMPOTENT_METHODS.has(method)) {
      return true;
    }
    // POST/PATCH are only retried when an idempotency key was provided, or
    // when the server explicitly told us to back off (429).
    if (opts.idempotencyKey) {
      return true;
    }
    return status === 429;
  }

  private async send(
    method: string,
    path: string,
    opts: RequestOptions,
  ): Promise<Response> {
    const url = this.buildUrl(path, opts.query);
    const timeoutMs = opts.timeoutMs ?? this.timeoutMs;
    let lastError: ShoalError | undefined;

    for (let attempt = 0; attempt <= this.maxRetries; attempt++) {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), timeoutMs);
      const onExternalAbort = () => controller.abort();
      if (opts.signal) {
        if (opts.signal.aborted) {
          clearTimeout(timer);
          throw new TimeoutError(`request aborted before sending: ${method} ${path}`);
        }
        opts.signal.addEventListener("abort", onExternalAbort, { once: true });
      }

      const headers: Record<string, string> = {
        accept: "application/json",
        "user-agent": this.userAgent,
        ...this.extraHeaders,
      };
      if (this.apiKey) {
        headers["authorization"] = `Bearer ${this.apiKey}`;
      }
      if (opts.idempotencyKey) {
        headers["idempotency-key"] = opts.idempotencyKey;
      }
      let body: string | undefined;
      if (opts.body !== undefined) {
        headers["content-type"] = "application/json";
        body = JSON.stringify(opts.body);
      }

      let response: Response | undefined;
      try {
        response = await this.fetchImpl(url, {
          method,
          headers,
          body,
          signal: controller.signal,
        });
      } catch (err) {
        const aborted =
          (err instanceof Error && err.name === "AbortError") ||
          controller.signal.aborted;
        lastError = aborted
          ? new TimeoutError(
              `request timed out after ${timeoutMs} ms: ${method} ${path}`,
            )
          : new NetworkError(`request failed: ${method} ${path}`, { cause: err });
        if (
          attempt < this.maxRetries &&
          !opts.signal?.aborted &&
          this.isRetryable(method, opts)
        ) {
          await this.sleep(this.backoffDelay(attempt));
          continue;
        }
        throw attempt > 0
          ? new RetriesExhaustedError(attempt + 1, lastError)
          : lastError;
      } finally {
        clearTimeout(timer);
        opts.signal?.removeEventListener("abort", onExternalAbort);
      }

      if (response.ok) {
        return response;
      }

      // Map the error response.
      let errBody: ApiErrorBody | undefined;
      try {
        const text = await response.text();
        if (text.length > 0) {
          errBody = JSON.parse(text) as ApiErrorBody;
        }
      } catch {
        // Non-JSON error body; keep undefined.
      }
      const apiError = errorFromResponse(response.status, errBody, response.headers);
      lastError = apiError;

      if (
        attempt < this.maxRetries &&
        this.isRetryable(method, opts, response.status)
      ) {
        const retryAfterMs =
          apiError instanceof RateLimitError ? apiError.retryAfterMs : undefined;
        await this.sleep(this.backoffDelay(attempt, retryAfterMs));
        continue;
      }

      if (attempt > 0 && (apiError instanceof ServerError || apiError instanceof RateLimitError)) {
        throw new RetriesExhaustedError(attempt + 1, apiError);
      }
      throw apiError;
    }

    // Unreachable, but keeps TypeScript satisfied.
    throw lastError ?? new ShoalError(`request failed: ${method} ${path}`);
  }
}

export { ApiError };
