"""Evaluate Pensyve against a benchmark dataset."""
import json
import time
import tempfile
import pensyve


def evaluate(benchmark_path: str, verbose: bool = False) -> dict:
    """Run Pensyve against a benchmark and return scores."""
    with open(benchmark_path) as f:
        benchmark = json.load(f)

    with tempfile.TemporaryDirectory() as tmpdir:
        p = pensyve.Pensyve(path=tmpdir)
        agent = p.entity("benchmark_agent", kind="agent")
        user = p.entity("benchmark_user", kind="user")

        # Phase 1: Ingest all conversations
        ingest_start = time.time()
        for conv in benchmark["conversations"]:
            with p.episode(agent, user) as ep:
                for msg in conv["messages"]:
                    ep.message(msg["role"], msg["content"])
        ingest_time = time.time() - ingest_start

        # Phase 2: Run all queries
        recall_start = time.time()
        results = []
        for query_item in benchmark["queries"]:
            query = query_item["query"]
            expected = query_item["expected_keywords"]

            memories = p.recall(query, entity=user, limit=5)

            # Check if any expected keyword appears in any returned memory
            recalled_text = " ".join([m.content.lower() for m in memories])
            hit = any(kw in recalled_text for kw in expected)

            results.append({
                "query": query,
                "expected": expected,
                "hit": hit,
                "num_results": len(memories),
                "top_content": memories[0].content if memories else None,
            })

            if verbose and not hit:
                print(f"  MISS: {query}")
                print(f"    Expected: {expected}")
                print(f"    Got: {recalled_text[:100]}")

        recall_time = time.time() - recall_start

    # Compute metrics
    hits = sum(1 for r in results if r["hit"])
    total = len(results)
    accuracy = hits / total if total > 0 else 0

    avg_recall_ms = (recall_time / total * 1000) if total > 0 else 0

    report = {
        "benchmark": benchmark["metadata"],
        "accuracy": round(accuracy * 100, 1),
        "hits": hits,
        "total_queries": total,
        "misses": total - hits,
        "ingest_time_s": round(ingest_time, 2),
        "recall_time_s": round(recall_time, 2),
        "avg_recall_ms": round(avg_recall_ms, 1),
        "details": results,
    }

    return report


if __name__ == "__main__":
    report = evaluate("benchmarks/results/synthetic_benchmark.json", verbose=True)
    print(f"\n{'='*50}")
    print(f"Pensyve Synthetic Benchmark Results")
    print(f"{'='*50}")
    print(f"Accuracy: {report['accuracy']}% ({report['hits']}/{report['total_queries']})")
    print(f"Ingest time: {report['ingest_time_s']}s")
    print(f"Avg recall: {report['avg_recall_ms']}ms")

    with open("benchmarks/results/synthetic_results.json", "w") as f:
        json.dump(report, f, indent=2)
