#!/usr/bin/env python3
"""Weight tuning script for Pensyve retrieval optimization.

Uses scipy.optimize.differential_evolution to find the optimal 8-signal
weight vector that maximizes accuracy on the LongMemEval_S benchmark.

NOTE: The Python SDK does not yet expose a `weights` parameter on Pensyve().
Until that is added, the optimizer evaluates the default compiled-in weights
on each iteration. The infrastructure is ready for when weights become
configurable -- just wire `weights` through `evaluate()` -> `Pensyve()`.

Usage:
    python benchmarks/tuning/optimize.py [--maxiter N] [--seed N] [--verbose]

The 8 signals are: vector, bm25, graph, intent, recency, access, confidence, type_boost.
"""

from __future__ import annotations

import argparse
import os
import sys
import time

# Ensure the project root is on sys.path when run as a script.
_project_root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
if _project_root not in sys.path:
    sys.path.insert(0, _project_root)

import numpy as np
from scipy.optimize import differential_evolution

from benchmarks.longmemeval.dataset import load_longmemeval_s
from benchmarks.longmemeval.evaluate import evaluate

SIGNAL_NAMES = [
    "vector",
    "bm25",
    "graph",
    "intent",
    "recency",
    "access",
    "confidence",
    "type_boost",
]

# Default weights from config.rs for reference.
DEFAULT_WEIGHTS = [0.25, 0.10, 0.15, 0.05, 0.20, 0.10, 0.10, 0.05]

# Each weight is bounded in [0, 1]; normalization ensures they sum to 1.
BOUNDS = [(0.0, 1.0)] * 8


def normalize_weights(raw: np.ndarray) -> list[float]:
    """Normalize weight vector to sum to 1.0."""
    total = np.sum(raw)
    if total < 1e-10:
        # Avoid division by zero; return uniform weights.
        return [1.0 / len(raw)] * len(raw)
    return (raw / total).tolist()


# Global counter for progress reporting.
_eval_count = 0
_best_accuracy = 0.0
_verbose = False


def objective(raw_weights: np.ndarray) -> float:
    """Objective function for optimization (minimized, so we negate accuracy).

    Args:
        raw_weights: Raw 8-element weight vector (will be normalized).

    Returns:
        Negative accuracy (since differential_evolution minimizes).
    """
    global _eval_count, _best_accuracy  # noqa: PLW0603

    weights = normalize_weights(raw_weights)
    _eval_count += 1

    dataset = load_longmemeval_s()
    report = evaluate(dataset, recall_limit=10, weights=weights)

    if report.accuracy > _best_accuracy:
        _best_accuracy = report.accuracy

    if _verbose and _eval_count % 10 == 0:
        print(
            f"  [iter {_eval_count:4d}] accuracy={report.accuracy:.1%} "
            f"best={_best_accuracy:.1%}"
        )

    return -report.accuracy


def main() -> int:
    global _verbose  # noqa: PLW0603

    parser = argparse.ArgumentParser(
        description="Optimize Pensyve retrieval weights via differential evolution"
    )
    parser.add_argument(
        "--maxiter",
        type=int,
        default=20,
        help="Maximum number of DE iterations (default: 20)",
    )
    parser.add_argument(
        "--seed",
        type=int,
        default=42,
        help="Random seed for reproducibility (default: 42)",
    )
    parser.add_argument(
        "--popsize",
        type=int,
        default=10,
        help="DE population size multiplier (default: 10)",
    )
    parser.add_argument(
        "--verbose",
        action="store_true",
        help="Print progress during optimization",
    )
    args = parser.parse_args()
    _verbose = args.verbose

    print("Pensyve Weight Tuning")
    print(f"{'=' * 60}")
    print(f"  Signals: {', '.join(SIGNAL_NAMES)}")
    print(f"  Default: {DEFAULT_WEIGHTS}")
    print(f"  Max iterations: {args.maxiter}")
    print(f"  Population size: {args.popsize}")
    print(f"  Seed: {args.seed}")

    # Evaluate default weights first as a baseline.
    print("\nEvaluating default weights...")
    dataset = load_longmemeval_s()
    baseline = evaluate(dataset, recall_limit=10)
    print(f"  Baseline accuracy: {baseline.accuracy:.1%} ({baseline.hits}/{baseline.total_queries})")

    print("\nRunning differential evolution...")
    start = time.perf_counter()

    result = differential_evolution(
        objective,
        bounds=BOUNDS,
        maxiter=args.maxiter,
        seed=args.seed,
        popsize=args.popsize,
        tol=0.001,
        mutation=(0.5, 1.5),
        recombination=0.7,
        disp=args.verbose,
    )

    elapsed = time.perf_counter() - start

    best_weights = normalize_weights(result.x)
    best_accuracy = -result.fun

    print(f"\n{'=' * 60}")
    print("Optimization Results")
    print(f"{'=' * 60}")
    print(f"  Evaluations: {_eval_count}")
    print(f"  Time:        {elapsed:.1f}s")
    print(f"\n  Baseline accuracy: {baseline.accuracy:.1%}")
    print(f"  Optimized accuracy: {best_accuracy:.1%}")
    improvement = best_accuracy - baseline.accuracy
    print(f"  Improvement:        {improvement:+.1%}")

    print("\n  Optimized weights:")
    for name, w in zip(SIGNAL_NAMES, best_weights):
        default_w = DEFAULT_WEIGHTS[SIGNAL_NAMES.index(name)]
        delta = w - default_w
        print(f"    {name:12s}: {w:.4f}  (default: {default_w:.4f}, delta: {delta:+.4f})")

    # Output as a Rust array literal for easy copy-paste.
    rust_array = ", ".join(f"{w:.4f}" for w in best_weights)
    print(f"\n  Rust config literal:")
    print(f"    weights: [{rust_array}]")

    return 0


if __name__ == "__main__":
    sys.exit(main())
