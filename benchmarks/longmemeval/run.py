#!/usr/bin/env python3
"""CLI runner for the LongMemEval_S benchmark.

Usage:
    python benchmarks/longmemeval/run.py [--data-dir DIR] [--verbose] [--limit N]
"""

from __future__ import annotations

import argparse
import os
import sys

# Ensure the project root is on sys.path when run as a script.
_project_root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
if _project_root not in sys.path:
    sys.path.insert(0, _project_root)

from benchmarks.longmemeval.dataset import load_longmemeval_s
from benchmarks.longmemeval.evaluate import evaluate


def main() -> int:
    parser = argparse.ArgumentParser(description="Run LongMemEval_S benchmark")
    parser.add_argument(
        "--data-dir",
        default=None,
        help="Directory containing conversations.json and queries.json",
    )
    parser.add_argument(
        "--verbose",
        action="store_true",
        help="Print per-query results",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=10,
        help="Number of memories to recall per query (default: 10)",
    )
    args = parser.parse_args()

    print("Loading dataset...")
    dataset = load_longmemeval_s(args.data_dir)
    print(
        f"  {dataset.num_conversations} conversations, "
        f"{dataset.num_queries} queries"
    )

    print("\nRunning evaluation...")
    report = evaluate(dataset, recall_limit=args.limit)

    print(f"\n{'=' * 60}")
    print("LongMemEval_S Benchmark Results")
    print(f"{'=' * 60}")
    print(f"  Accuracy:      {report.accuracy:.1%} ({report.hits}/{report.total_queries})")
    print(f"  Hits:          {report.hits}")
    print(f"  Misses:        {report.misses}")
    print(f"  Ingest time:   {report.ingest_time_ms:.1f} ms")
    print(f"  Query time:    {report.query_time_ms:.1f} ms")
    print(f"  Avg query:     {report.query_time_ms / max(report.total_queries, 1):.1f} ms")

    if args.verbose:
        print(f"\n{'=' * 60}")
        print("Per-query results:")
        print(f"{'=' * 60}")
        for r in report.results:
            status = "HIT " if r.hit else "MISS"
            print(f"  [{status}] {r.query_id}: {r.question}")
            print(f"         Gold: {r.gold_answer}")
            if not r.hit:
                preview = "; ".join(
                    t[:60] for t in r.recalled_texts[:3]
                )
                print(f"         Got:  {preview}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
