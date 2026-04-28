"""Smoke tests for the Phase C.2 prewarm + cached-extractor binding.

The Phase C.2 cost-opt path lives in three layers: pensyve-core's
`CachedBulkExtractor`, a thin pyo3 binding here that exposes
`prewarm_haiku_extraction_cache` + `Pensyve(extractor="haiku-cached", ...)`,
and a wave runner in `agent-memory-bench` that orchestrates the prewarm.
This file covers the pyo3 binding in isolation — the prewarm path is
exercised in the Rust wiremock test (no live API spend), so here we only
verify:

1. The `HaikuExtractionCache` class is importable and has the documented
   `__len__` + `size()` surface.
2. `Pensyve(extractor="haiku-cached", extractor_cache=None)` rejects the
   misconfiguration with a clear error (regression for the kwarg wiring).
3. `Pensyve(extractor="haiku-cached", extractor_cache=<empty cache>)` is
   acceptable — the wrapper falls through to the inner extractor on every
   `extract` call, but constructing it must succeed so a wave runner can
   audit empty-cache misconfig before running ingest.

A real prewarm round-trip would require live Anthropic API access, which
violates the `no live API spend` constraint. The pensyve-core wiremock
test (`prewarm_then_cached_extract_makes_exactly_one_batch_post`) already
validates the full HTTP wire shape end-to-end.
"""

from __future__ import annotations

from pathlib import Path

import pytest

# Compiled extension is optional in pure-Python CI matrices.
pensyve = pytest.importorskip("pensyve")


def test_cache_class_exposes_documented_surface() -> None:
    """`HaikuExtractionCache` is exported and has __len__ + size()."""
    # Importable from the public namespace.
    assert hasattr(pensyve, "HaikuExtractionCache")
    # Empty-construction path: prewarm with zero items returns an empty cache.
    empty_cache = pensyve.prewarm_haiku_extraction_cache([])
    # Documented size accessors.
    assert len(empty_cache) == 0
    assert empty_cache.size() == 0


def test_haiku_cached_requires_extractor_cache_kwarg(tmp_path: Path) -> None:
    """`extractor="haiku-cached"` without a cache must raise a clear error.

    Regression for the kwarg wiring — without the validation the constructor
    would silently build a cache-less extractor and every `extract()` call
    would fall through to the fallback, defeating the whole Phase C.2 path
    without a visible failure.
    """
    with pytest.raises((ValueError, TypeError)) as exc_info:
        pensyve.Pensyve(
            path=str(tmp_path),
            namespace="t-cached-no-cache",
            extractor="haiku-cached",
        )
    msg = str(exc_info.value).lower()
    assert "extractor_cache" in msg or "haiku-cached" in msg, (
        f"error message must mention extractor_cache or haiku-cached, got: {msg}"
    )


def test_haiku_cached_constructs_with_empty_cache_and_api_key(tmp_path: Path) -> None:
    """Empty cache is valid construction-time input.

    Pensyve(extractor="haiku-cached", extractor_cache=<empty>) succeeds.
    The wrapper exists; it falls through to the inner Haiku extractor on
    every `extract()` because the cache has zero entries — that's the
    "wave runner forgot to prewarm" case, surfaced as warnings at runtime
    rather than a constructor error so the operator can still make
    forward progress on a misconfigured wave.

    Constructing the inner Haiku fallback requires an API key, which we
    pass explicitly to avoid depending on a real ANTHROPIC_API_KEY env var.
    """
    empty_cache = pensyve.prewarm_haiku_extraction_cache([])
    p = pensyve.Pensyve(
        path=str(tmp_path),
        namespace="t-cached-empty",
        extractor="haiku-cached",
        extractor_api_key="dummy-key-for-construction-only",
        extractor_cache=empty_cache,
        # Skip the BGE reranker download in this smoke test so it runs in
        # restricted environments without a model cache.
        reranker=None,
    )
    # Sanity: the constructed Pensyve is usable for entity creation. Calling
    # `extract()` would require a real API call, which the constraints
    # forbid; we stop at construction.
    assert p.entity("user") is not None
