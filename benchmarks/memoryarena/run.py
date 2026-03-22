"""MemoryArena benchmark CLI runner."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description="Run MemoryArena benchmark")
    parser.add_argument("--limit", type=int, default=10, help="Recall limit")
    parser.add_argument("--verbose", action="store_true")
    parser.add_argument("--output", type=Path, help="Save results JSON")
    args = parser.parse_args()

    from .dataset import load_builtin
    from .evaluate import evaluate

    scenarios = load_builtin()
    print(f"Scenarios: {len(scenarios)}")
    print()

    results = evaluate(scenarios, limit=args.limit, verbose=args.verbose)

    print()
    print("MemoryArena Results")
    print("=" * 60)
    print(f"  Accuracy:    {results.accuracy:.1%} ({results.correct}/{results.total})")
    print(f"  Safety rate: {results.safety_rate:.1%}")
    print(f"  Ingest time: {results.ingest_time_ms:.1f} ms")
    print(f"  Query time:  {results.query_time_ms:.1f} ms")
    print()

    for category, stats in sorted(results.by_category.items()):
        acc = stats["correct"] / stats["total"] if stats["total"] > 0 else 0
        print(f"  {category:20s}: {acc:.1%} ({stats['correct']}/{stats['total']})")

    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        with open(args.output, "w") as f:
            json.dump({
                "accuracy": results.accuracy,
                "safety_rate": results.safety_rate,
                "correct": results.correct,
                "incorrect": results.incorrect,
                "neutral": results.neutral,
                "total": results.total,
                "by_category": results.by_category,
                "failures": results.failures,
            }, f, indent=2)
        print(f"\nResults saved to {args.output}")


if __name__ == "__main__":
    main()
