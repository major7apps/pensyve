"""Run Pensyve benchmarks."""

import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import json
import random

from synthetic.evaluate import evaluate
from synthetic.generate import generate_benchmark


def main():
    parser = argparse.ArgumentParser(description="Pensyve Benchmark Runner")
    parser.add_argument("--generate", action="store_true", help="Generate benchmark data")
    parser.add_argument("--evaluate", action="store_true", help="Evaluate Pensyve")
    parser.add_argument("--conversations", type=int, default=50, help="Number of conversations")
    parser.add_argument("--verbose", action="store_true", help="Show missed queries")
    parser.add_argument("--seed", type=int, default=42, help="Random seed")
    args = parser.parse_args()

    if args.generate or not os.path.exists("benchmarks/results/synthetic_benchmark.json"):
        random.seed(args.seed)
        data = generate_benchmark(num_conversations=args.conversations)
        os.makedirs("benchmarks/results", exist_ok=True)
        with open("benchmarks/results/synthetic_benchmark.json", "w") as f:
            json.dump(data, f, indent=2)
        print(
            f"Generated {len(data['conversations'])} conversations, {len(data['queries'])} queries"
        )

    if args.evaluate or not args.generate:
        report = evaluate("benchmarks/results/synthetic_benchmark.json", verbose=args.verbose)
        print(f"\nAccuracy: {report['accuracy']}% ({report['hits']}/{report['total_queries']})")
        print(f"Ingest: {report['ingest_time_s']}s | Avg recall: {report['avg_recall_ms']}ms")

        with open("benchmarks/results/synthetic_results.json", "w") as f:
            json.dump(report, f, indent=2)


if __name__ == "__main__":
    main()
