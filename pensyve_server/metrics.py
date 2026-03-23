"""Observability middleware and /metrics endpoint for the Pensyve REST API.

Tracks per-endpoint request counts and latencies, and exposes them in
Prometheus text exposition format. When the Rust-side pensyve._core module
is available, its metrics are merged into the output.
"""

from __future__ import annotations

import re
import time
from collections import defaultdict
from threading import Lock
from typing import TYPE_CHECKING

from fastapi import APIRouter, Request, Response
from starlette.middleware.base import BaseHTTPMiddleware

if TYPE_CHECKING:
    from collections.abc import Callable

# ---------------------------------------------------------------------------
# Server-side metrics state
# ---------------------------------------------------------------------------

_lock = Lock()
_request_counts: dict[str, int] = defaultdict(int)
_request_durations_ms: dict[str, float] = defaultdict(float)


def _normalize_path(path: str) -> str:
    """Replace path parameters with placeholders to prevent cardinality explosion."""
    # Replace UUIDs
    path = re.sub(r'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}', '{id}', path)
    # Replace entity names in /v1/entities/{name} and /v1/gdpr/erase/{name}
    path = re.sub(r'/v1/entities/[^/]+', '/v1/entities/{name}', path)
    path = re.sub(r'/v1/gdpr/erase/[^/]+', '/v1/gdpr/erase/{name}', path)
    return path


def _record_request(path: str, duration_ms: float) -> None:
    with _lock:
        _request_counts[path] += 1
        _request_durations_ms[path] += duration_ms


# ---------------------------------------------------------------------------
# Middleware
# ---------------------------------------------------------------------------


class MetricsMiddleware(BaseHTTPMiddleware):
    """Starlette middleware that records request count and latency per path."""

    async def dispatch(self, request: Request, call_next: Callable) -> Response:
        start = time.monotonic()
        response = await call_next(request)
        duration_ms = (time.monotonic() - start) * 1000.0
        # Normalize the path to avoid cardinality explosion from path params.
        path = _normalize_path(request.url.path)
        _record_request(path, duration_ms)
        return response


# ---------------------------------------------------------------------------
# /metrics endpoint
# ---------------------------------------------------------------------------

router = APIRouter()


def _server_prometheus_text() -> str:
    """Generate Prometheus text for server-side HTTP metrics."""
    lines: list[str] = []

    with _lock:
        counts = dict(_request_counts)
        durations = dict(_request_durations_ms)

    if counts:
        lines.append("# HELP pensyve_http_requests_total Total HTTP requests by path.")
        lines.append("# TYPE pensyve_http_requests_total counter")
        for path, count in sorted(counts.items()):
            safe_path = path.replace('"', '\\"')
            lines.append(f'pensyve_http_requests_total{{path="{safe_path}"}} {count}')

    if durations:
        lines.append(
            "# HELP pensyve_http_request_duration_ms_total "
            "Cumulative request duration in milliseconds by path."
        )
        lines.append("# TYPE pensyve_http_request_duration_ms_total counter")
        for path, total_ms in sorted(durations.items()):
            safe_path = path.replace('"', '\\"')
            lines.append(
                f'pensyve_http_request_duration_ms_total{{path="{safe_path}"}} {total_ms:.1f}'
            )

    return "\n".join(lines) + "\n" if lines else ""


def _rust_prometheus_text() -> str:
    """Fetch Prometheus metrics from the Rust core, if available."""
    try:
        import pensyve._core as core  # type: ignore[import-not-found]

        if hasattr(core, "prometheus_metrics"):
            return core.prometheus_metrics()  # type: ignore[attr-defined]
    except (ImportError, AttributeError):
        pass
    return ""


@router.get("/metrics", include_in_schema=False)
async def metrics_endpoint() -> Response:
    """Expose merged Prometheus metrics (server + Rust core)."""
    body = _server_prometheus_text() + _rust_prometheus_text()
    return Response(content=body, media_type="text/plain; charset=utf-8")
