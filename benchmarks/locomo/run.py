"""LoCoMo benchmark CLI runner."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description="Run LoCoMo benchmark")
    parser.add_argument("--data-dir", type=Path, help="External dataset directory")
    parser.add_argument("--limit", type=int, default=10, help="Recall limit per query")
    parser.add_argument("--verbose", action="store_true", help="Print per-query results")
    parser.add_argument("--output", type=Path, help="Save results to JSON file")
    args = parser.parse_args()

    from .dataset import load_builtin, load_external
    from .evaluate import evaluate

    if args.data_dir:
        print(f"Loading external dataset from {args.data_dir}")
        conversations, queries = load_external(args.data_dir)
    else:
        print("Using builtin LoCoMo test dataset")
        conversations, queries = load_builtin()

    print(f"Conversations: {len(conversations)}, Queries: {len(queries)}")
    print()

    results = evaluate(conversations, queries, limit=args.limit, verbose=args.verbose)

    print()
    print("LoCoMo Benchmark Results")
    print("=" * 60)
    print(f"  Overall Accuracy: {results.accuracy:.1%} ({results.hits}/{results.total})")
    print(f"  Ingest time:      {results.ingest_time_ms:.1f} ms")
    print(f"  Query time:       {results.query_time_ms:.1f} ms")
    print()

    for category, stats in sorted(results.by_category.items()):
        acc = stats["hits"] / stats["total"] if stats["total"] > 0 else 0
        print(f"  {category:15s}: {acc:.1%} ({stats['hits']}/{stats['total']})")

    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        with open(args.output, "w") as f:
            json.dump(
                {
                    "accuracy": results.accuracy,
                    "hits": results.hits,
                    "total": results.total,
                    "by_category": results.by_category,
                    "ingest_time_ms": results.ingest_time_ms,
                    "query_time_ms": results.query_time_ms,
                    "missed_queries": results.missed_queries,
                },
                f,
                indent=2,
            )
        print(f"\nResults saved to {args.output}")


if __name__ == "__main__":
    main()
