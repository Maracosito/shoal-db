"""Fixtures for the live-server e2e suite.

The whole suite is skipped unless SHOAL_E2E_URL is set, keeping unit-test CI
hermetic. See e2e/README.md for how to run against the Compose stack.
"""

from __future__ import annotations

import time
from typing import Callable, List

import pytest

from helpers import E2E_API_KEY, E2E_URL, unique_ns


def pytest_collection_modifyitems(config, items):  # noqa: ARG001
    if E2E_URL:
        return
    skip = pytest.mark.skip(
        reason="SHOAL_E2E_URL is not set; live-server e2e suite disabled"
    )
    for item in items:
        item.add_marker(skip)


@pytest.fixture(scope="session")
def base_url() -> str:
    """Wait for the server to become healthy, then hand out its URL."""
    import httpx

    deadline = time.monotonic() + 180.0
    last_err = "no attempt made"
    while time.monotonic() < deadline:
        try:
            resp = httpx.get(f"{E2E_URL}/healthz", timeout=2.0)
            if resp.status_code == 200:
                return E2E_URL
            last_err = f"/healthz returned HTTP {resp.status_code}"
        except Exception as exc:  # noqa: BLE001 - report any transport error
            last_err = repr(exc)
        time.sleep(1.0)
    pytest.fail(f"server at {E2E_URL} never became healthy: {last_err}")


@pytest.fixture(scope="session")
def client(base_url: str):
    from shoal import Client

    c = Client(base_url=base_url, api_key=E2E_API_KEY)
    yield c
    c.close()


@pytest.fixture()
def track(client) -> Callable[[str], str]:
    """Register namespace names for best-effort cleanup after the test."""
    names: List[str] = []

    def _track(name: str) -> str:
        names.append(name)
        return name

    yield _track

    for name in reversed(names):
        try:
            client.delete_namespace(name)
        except Exception:  # noqa: BLE001 - cleanup is best effort
            pass


@pytest.fixture()
def make_namespace(client, track) -> Callable[..., str]:
    """Factory creating uniquely named namespaces; returns the *name*."""

    def _make(prefix: str = "e2e-py") -> str:
        name = track(unique_ns(prefix))
        client.create_namespace(name)
        return name

    return _make


@pytest.fixture()
def namespace(client, make_namespace):
    """A fresh, empty namespace handle for a single test."""
    name = make_namespace()
    return client.namespace(name)
