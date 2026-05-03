"""Stress test: repeated `Pensyve(...)` construction must NOT leak memory.

Validates the embedder/reranker process-wide cache fix staged in
`pensyve-docs/research/benchmark-sprint/_leak_diagnosis.md`. Pre-fix the
ONNX session pool (4× GTE-base ≈ 1.3 GB) and BGE reranker (~250 MB) were
rebuilt every constructor and not returned to the OS allocator on Drop,
producing ~250 MB-per-iteration RSS growth. Post-fix the cache reuses
`Arc<OnnxEmbedder>` / `Arc<Reranker>` so RSS plateaus after iteration 1.

Run directly (not via pytest — this is a long-running diagnostic):

    conda run -n pensyve-longmemeval python pensyve-python/tests/stress_construct.py

Acceptance: peak RSS plateaus within ±100 MB after iteration 1; no linear
growth across 100 iterations.
"""

from __future__ import annotations

import gc
import resource
import tempfile

from pensyve import Pensyve


def rss_mb() -> float:
    return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024


def main(iterations: int = 100) -> None:
    print(f"stress_construct: {iterations} iterations, baseline RSS = {rss_mb():.0f} MB")
    for i in range(iterations):
        with tempfile.TemporaryDirectory() as td:
            p = Pensyve(path=td, namespace=f"ns{i}", reranker="BGERerankerBase")
            del p
            gc.collect()
        if i % 10 == 0 or i == iterations - 1:
            print(f"  i={i:3d} rss={rss_mb():6.0f} MB")
    print(f"stress_construct: done, final RSS = {rss_mb():.0f} MB")


if __name__ == "__main__":
    main()
