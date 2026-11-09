/**
 * Test helpers: a scripted mock fetch implementation that records requests
 * and returns canned responses in order.
 */

import type { FetchLike } from "../src/transport.js";

export interface RecordedRequest {
  url: string;
  method: string;
  headers: Record<string, string>;
  body?: unknown;
}

export type CannedResponse =
  | { status: number; body?: unknown; headers?: Record<string, string> }
  | { networkError: string }
  | { hang: true };

export interface MockFetch {
  fetch: FetchLike;
  requests: RecordedRequest[];
}

export function mockFetch(responses: CannedResponse[]): MockFetch {
  const requests: RecordedRequest[] = [];
  let call = 0;

  const fetch: FetchLike = async (input, init) => {
    const headers: Record<string, string> = {};
    const rawHeaders = init?.headers as Record<string, string> | undefined;
    if (rawHeaders) {
      for (const [k, v] of Object.entries(rawHeaders)) {
        headers[k.toLowerCase()] = v;
      }
    }
    requests.push({
      url: String(input),
      method: init?.method ?? "GET",
      headers,
      body: typeof init?.body === "string" ? JSON.parse(init.body) : undefined,
    });

    const canned = responses[Math.min(call, responses.length - 1)];
    call += 1;
    if (!canned) {
      throw new Error("mockFetch: no canned response configured");
    }
    if ("networkError" in canned) {
      throw new TypeError(canned.networkError);
    }
    if ("hang" in canned) {
      // Resolve only when aborted, simulating a stalled connection.
      return new Promise<Response>((_resolve, reject) => {
        const signal = init?.signal;
        if (signal?.aborted) {
          const err = new Error("aborted");
          err.name = "AbortError";
          reject(err);
          return;
        }
        signal?.addEventListener("abort", () => {
          const err = new Error("aborted");
          err.name = "AbortError";
          reject(err);
        });
      });
    }
    const bodyText =
      canned.body === undefined ? null : JSON.stringify(canned.body);
    return new Response(bodyText, {
      status: canned.status,
      headers: {
        "content-type": "application/json",
        ...canned.headers,
      },
    });
  };

  return { fetch, requests };
}

/** A sleep that resolves immediately, so retry tests run fast. */
export const instantSleep = (_ms: number): Promise<void> => Promise.resolve();
