"""Exception hierarchy for the Shoal Python SDK.

All exceptions raised by the SDK inherit from :class:`ShoalError`, so callers
can catch a single base class. HTTP-level failures are mapped onto typed
subclasses of :class:`APIError`; network failures (DNS, connect, timeout)
are raised as :class:`TransportError`.
"""

from __future__ import annotations

from typing import Any, Dict, Optional

import httpx


class ShoalError(Exception):
    """Base class for every error raised by the Shoal SDK."""


class TransportError(ShoalError):
    """A network-level failure (connection refused, DNS failure, timeout)
    that persisted after all configured retries were exhausted."""


class APIError(ShoalError):
    """An error response returned by the Shoal API server."""

    def __init__(
        self,
        message: str,
        *,
        status_code: int,
        code: Optional[str] = None,
        request_id: Optional[str] = None,
        body: Optional[Dict[str, Any]] = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.status_code = status_code
        self.code = code
        self.request_id = request_id
        self.body = body or {}

    def __str__(self) -> str:
        parts = [f"[HTTP {self.status_code}]"]
        if self.code:
            parts.append(f"({self.code})")
        parts.append(self.message)
        if self.request_id:
            parts.append(f"request_id={self.request_id}")
        return " ".join(parts)


class BadRequestError(APIError):
    """The request was malformed or failed validation (HTTP 400 / 422)."""


class AuthenticationError(APIError):
    """The API key is missing or invalid (HTTP 401)."""


class AuthorizationError(APIError):
    """The API key lacks the role required for this operation (HTTP 403)."""


class NotFoundError(APIError):
    """The namespace or document does not exist (HTTP 404)."""


class ConflictError(APIError):
    """The operation conflicted with current state, e.g. a failed
    conditional write or a namespace that already exists (HTTP 409)."""


class RateLimitError(APIError):
    """The request was rejected due to rate limits or quotas (HTTP 429)."""

    def __init__(self, message: str, *, retry_after: Optional[float] = None, **kwargs: Any) -> None:
        super().__init__(message, **kwargs)
        self.retry_after = retry_after


class ServerError(APIError):
    """The server failed to process the request (HTTP 5xx)."""


def _parse_retry_after(response: httpx.Response) -> Optional[float]:
    value = response.headers.get("Retry-After")
    if value is None:
        return None
    try:
        return max(0.0, float(value))
    except ValueError:
        return None


def error_from_response(response: httpx.Response) -> APIError:
    """Build the appropriate typed exception from an error response."""
    status = response.status_code
    code: Optional[str] = None
    request_id = response.headers.get("X-Request-Id")
    body: Dict[str, Any] = {}
    message = f"request failed with status {status}"

    try:
        parsed = response.json()
        if isinstance(parsed, dict):
            body = parsed
            err = parsed.get("error")
            if isinstance(err, dict):
                code = err.get("code") or code
                message = err.get("message") or message
                request_id = err.get("request_id") or request_id
            elif isinstance(err, str):
                message = err
            elif "message" in parsed:
                message = str(parsed["message"])
    except Exception:
        text = response.text.strip()
        if text:
            message = text[:500]

    kwargs: Dict[str, Any] = {
        "status_code": status,
        "code": code,
        "request_id": request_id,
        "body": body,
    }

    if status in (400, 422):
        return BadRequestError(message, **kwargs)
    if status == 401:
        return AuthenticationError(message, **kwargs)
    if status == 403:
        return AuthorizationError(message, **kwargs)
    if status == 404:
        return NotFoundError(message, **kwargs)
    if status == 409:
        return ConflictError(message, **kwargs)
    if status == 429:
        return RateLimitError(message, retry_after=_parse_retry_after(response), **kwargs)
    if status >= 500:
        return ServerError(message, **kwargs)
    return APIError(message, **kwargs)


def raise_for_response(response: httpx.Response) -> None:
    """Raise a typed :class:`APIError` if the response indicates failure."""
    if response.status_code < 400:
        return
    raise error_from_response(response)
