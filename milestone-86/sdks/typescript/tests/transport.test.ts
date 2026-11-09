import { describe, expect, it } from "vitest";
import {
  AuthenticationError,
  ConflictError,
  NotFoundError,
  RateLimitError,
  RetriesExhaustedError,
  ServerError,
  TimeoutError,
  ValidationError,
} from "../src/errors.js";
import { Transport } from "../src/transport.js";
import { instantSleep, mockFetch } from "./helpers.js";

function makeTransport(
  mock: ReturnType<typeof mockFetch>,
  overrides: Partial<ConstructorParameters<typeof Transport>[0]> = {},
) {
  return new Transport({
    baseUrl: "http://test.local",
    apiKey: "sk-test",
    fetch: mock.fetch,
    sleep: instantSleep,
    maxRetries: 3,
    ...overrides,
  });
}

describe("Transport", () => {
  it("sends auth and content-type headers and parses JSON", async () => {
    const mock = mockFetch([{ status: 200, body: { ok: true } }]);
    const t = makeTransport(mock);
    const res = await t.request<{ ok: boolean }>("POST", "/v1/thing", {
      body: { a: 1 },
    });
    expect(res).toEqual({ ok: true });
    const req = mock.requests[0]!;
    expect(req.method).toBe("POST");
    expect(req.url).toBe("http://test.local/v1/thing");
    expect(req.headers["authorization"]).toBe("Bearer sk-test");
    expect(req.headers["content-type"]).toBe("application/json");
    expect(req.body).toEqual({ a: 1 });
  });

  it("serializes query parameters and drops undefined", async () => {
    const mock = mockFetch([{ status: 200, body: {} }]);
    const t = makeTransport(mock);
    await t.request("GET", "/v1/x", {
      query: { limit: 10, cursor: undefined, prefix: "ab" },
    });
    const url = new URL(mock.requests[0]!.url);
    expect(url.searchParams.get("limit")).toBe("10");
    expect(url.searchParams.get("prefix")).toBe("ab");
    expect(url.searchParams.has("cursor")).toBe(false);
  });

  it("retries GET on 503 then succeeds", async () => {
    const mock = mockFetch([
      { status: 503, body: { error: { message: "busy" } } },
      { status: 503, body: { error: { message: "busy" } } },
      { status: 200, body: { ok: 1 } },
    ]);
    const t = makeTransport(mock);
    const res = await t.request<{ ok: number }>("GET", "/v1/x");
    expect(res.ok).toBe(1);
    expect(mock.requests.length).toBe(3);
  });

  it("does not retry POST without an idempotency key on 500", async () => {
    const mock = mockFetch([
      { status: 500, body: { error: { message: "boom" } } },
    ]);
    const t = makeTransport(mock);
    await expect(t.request("POST", "/v1/x", { body: {} })).rejects.toBeInstanceOf(
      ServerError,
    );
    expect(mock.requests.length).toBe(1);
  });

  it("retries POST when an idempotency key is provided", async () => {
    const mock = mockFetch([
      { status: 502, body: { error: { message: "bad gateway" } } },
      { status: 200, body: { ok: true } },
    ]);
    const t = makeTransport(mock);
    const res = await t.request<{ ok: boolean }>("POST", "/v1/x", {
      body: {},
      idempotencyKey: "key-1",
    });
    expect(res.ok).toBe(true);
    expect(mock.requests.length).toBe(2);
    expect(mock.requests[0]!.headers["idempotency-key"]).toBe("key-1");
    expect(mock.requests[1]!.headers["idempotency-key"]).toBe("key-1");
  });

  it("retries POST on 429 and honors Retry-After", async () => {
    const slept: number[] = [];
    const mock = mockFetch([
      {
        status: 429,
        body: { error: { message: "slow down" } },
        headers: { "retry-after": "1" },
      },
      { status: 200, body: { ok: true } },
    ]);
    const t = makeTransport(mock, {
      sleep: async (ms) => {
        slept.push(ms);
      },
    });
    const res = await t.request<{ ok: boolean }>("POST", "/v1/x", { body: {} });
    expect(res.ok).toBe(true);
    expect(slept).toEqual([1000]);
  });

  it("throws RetriesExhaustedError after persistent 503", async () => {
    const mock = mockFetch([
      { status: 503, body: { error: { message: "down" } } },
    ]);
    const t = makeTransport(mock, { maxRetries: 2 });
    const err = await t.request("GET", "/v1/x").catch((e) => e);
    expect(err).toBeInstanceOf(RetriesExhaustedError);
    expect((err as RetriesExhaustedError).attempts).toBe(3);
    expect(mock.requests.length).toBe(3);
  });

  it("maps status codes to error classes", async () => {
    const cases: Array<[number, unknown]> = [
      [400, ValidationError],
      [401, AuthenticationError],
      [403, AuthenticationError],
      [404, NotFoundError],
      [409, ConflictError],
      [422, ValidationError],
    ];
    for (const [status, cls] of cases) {
      const mock = mockFetch([
        { status, body: { error: { message: "x", code: "test" } } },
      ]);
      const t = makeTransport(mock);
      const err = await t.request("POST", "/v1/x", { body: {} }).catch((e) => e);
      expect(err).toBeInstanceOf(cls as never);
      expect((err as { code?: string }).code).toBe("test");
    }
  });

  it("exposes retryAfterMs on RateLimitError", async () => {
    const mock = mockFetch([
      {
        status: 429,
        body: { error: { message: "limited" } },
        headers: { "retry-after": "2" },
      },
    ]);
    const t = makeTransport(mock, { maxRetries: 0 });
    const err = await t.request("GET", "/v1/x").catch((e) => e);
    expect(err).toBeInstanceOf(RateLimitError);
    expect((err as RateLimitError).retryAfterMs).toBe(2000);
  });

  it("retries network errors on idempotent methods", async () => {
    const mock = mockFetch([
      { networkError: "fetch failed" },
      { status: 200, body: { ok: true } },
    ]);
    const t = makeTransport(mock);
    const res = await t.request<{ ok: boolean }>("GET", "/v1/x");
    expect(res.ok).toBe(true);
    expect(mock.requests.length).toBe(2);
  });

  it("times out hung requests", async () => {
    const mock = mockFetch([{ hang: true }]);
    const t = makeTransport(mock, { timeoutMs: 20, maxRetries: 0 });
    const err = await t.request("GET", "/v1/x").catch((e) => e);
    expect(err).toBeInstanceOf(TimeoutError);
  });

  it("handles 204 responses", async () => {
    const mock = mockFetch([{ status: 204 }]);
    const t = makeTransport(mock);
    const res = await t.request<void>("DELETE", "/v1/x");
    expect(res).toBeUndefined();
  });
});
