import json
from typing import Callable, Optional

import httpx

from shoal import RetryConfig, Shoal
from shoal.aio import AsyncShoal


def fast_retry(max_retries: int = 2) -> RetryConfig:
    return RetryConfig(max_retries=max_retries, initial_backoff=0.0, jitter=False)


def make_client(
    handler: Callable[[httpx.Request], httpx.Response],
    *,
    api_key: Optional[str] = "test-key",
    retry: Optional[RetryConfig] = None,
) -> Shoal:
    transport = httpx.MockTransport(handler)
    http = httpx.Client(transport=transport)
    return Shoal(
        api_key=api_key,
        base_url="http://testserver",
        http_client=http,
        retry=retry or fast_retry(),
    )


def make_async_client(
    handler: Callable[[httpx.Request], httpx.Response],
    *,
    api_key: Optional[str] = "test-key",
    retry: Optional[RetryConfig] = None,
) -> AsyncShoal:
    transport = httpx.MockTransport(handler)
    http = httpx.AsyncClient(transport=transport)
    return AsyncShoal(
        api_key=api_key,
        base_url="http://testserver",
        http_client=http,
        retry=retry or fast_retry(),
    )


def body_of(request: httpx.Request) -> dict:
    return json.loads(request.content.decode("utf-8"))
