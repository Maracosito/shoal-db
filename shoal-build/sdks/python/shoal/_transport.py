"""Retrying HTTP transport for sync and async clients.

Retries connection errors, timeouts, and retryable HTTP statuses
(408, 429, 5xx) with exponential backoff plus optional jitter, honoring
`Retry-After` response headers when present.

Note: write requests are retried as well; pair them with an idempotency key
(`idempotency_key=` on upsert/patch/delete) so retries are safe.
"""

from __future__ import annotations

import asyncio
import random
import time
from dataclasses import dataclass, field
from typing import FrozenSet, Optional

import httpx

from .errors import TransportError

DEFAULT_RETRY_STATUSES: FrozenSet[int] = frozenset({408, 429, 500, 502, 503, 504})


@dataclass
class RetryConfig:
    """Configuration for the SDK's retry behavior."""

    max_retries: int = 3
    initial_backoff: float = 0.25
    max_backoff: float = 8.0
    multiplier: float = 2.0
    jitter: bool = True
    retry_statuses: FrozenSet[int] = field(default_factory=lambda: DEFAULT_RETRY_STATUSES)


def parse_retry_after(response: httpx.Response) -> Optional[float]:
    value = response.headers.get("Retry-After")
    if value is None:
        return None
    try:
        return max(0.0, float(value))
    except ValueError:
        return None


def compute_backoff(attempt: int, config: RetryConfig, retry_after: Optional[float] = None) -> float:
    """Compute the sleep before retry number `attempt + 1` (0-indexed)."""
    if retry_after is not None:
        return min(retry_after, config.max_backoff)
    delay = config.initial_backoff * (config.multiplier ** attempt)
    delay = min(delay, config.max_backoff)
    if config.jitter:
        delay *= 0.5 + random.random() / 2.0
    return delay


def send_with_retries(
    client: httpx.Client,
    request: httpx.Request,
    config: RetryConfig,
) -> httpx.Response:
    attempt = 0
    while True:
        try:
            response = client.send(request)
        except (httpx.TimeoutException, httpx.TransportError) as exc:
            if attempt >= config.max_retries:
                raise TransportError(
                    f"{type(exc).__name__} after {attempt + 1} attempt(s): {exc}"
                ) from exc
            time.sleep(compute_backoff(attempt, config))
            attempt += 1
            continue

        if response.status_code in config.retry_statuses and attempt < config.max_retries:
            retry_after = parse_retry_after(response)
            response.close()
            time.sleep(compute_backoff(attempt, config, retry_after))
            attempt += 1
            continue

        return response


async def send_with_retries_async(
    client: httpx.AsyncClient,
    request: httpx.Request,
    config: RetryConfig,
) -> httpx.Response:
    attempt = 0
    while True:
        try:
            response = await client.send(request)
        except (httpx.TimeoutException, httpx.TransportError) as exc:
            if attempt >= config.max_retries:
                raise TransportError(
                    f"{type(exc).__name__} after {attempt + 1} attempt(s): {exc}"
                ) from exc
            await asyncio.sleep(compute_backoff(attempt, config))
            attempt += 1
            continue

        if response.status_code in config.retry_statuses and attempt < config.max_retries:
            retry_after = parse_retry_after(response)
            await response.aclose()
            await asyncio.sleep(compute_backoff(attempt, config, retry_after))
            attempt += 1
            continue

        return response
